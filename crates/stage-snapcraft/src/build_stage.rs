use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;

use crate::arch::{is_valid_snap_arch, triple_to_snap_arch};
use crate::command::snapcraft_command;
use crate::generate::generate_snap_yaml;
use crate::yaml::DEFAULT_SNAP_NAME_TEMPLATE;

// snapcraft ≤8.14.5: `snapcraft_legacy.internal.repo._deb` runs
// `BaseDirectory.save_cache_path("snapcraft", "download")` at import time, which
// calls `os.makedirs(path)` without `exist_ok=True`. Once the first invocation
// creates that directory, every subsequent snapcraft process crashes at import
// before it can pack. We wipe the cache dir and serialize invocations so the
// wipe-then-pack sequence is atomic across parallel workers.
static SNAPCRAFT_CACHE_LOCK: Mutex<()> = Mutex::new(());

/// Resolve the snapcraft download-cache directory from XDG/HOME semantics.
///
/// `XDG_CACHE_HOME` wins when set (snapcraft uses `xdg.BaseDirectory`,
/// which honors it); otherwise falls back to `<home>/.cache`. Returns the
/// exact `…/snapcraft/download` leaf — never a parent — so a wipe can
/// only ever touch the directory snapcraft re-creates at import time.
fn snapcraft_download_cache(xdg_cache_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_cache_home.filter(|s| !s.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|s| !s.is_empty())?).join(".cache"),
    };
    Some(base.join("snapcraft").join("download"))
}

/// Wipe snapcraft's download cache to dodge the ≤8.14.5 import-time crash
/// (see `SNAPCRAFT_CACHE_LOCK`). Only removes the `…/snapcraft/download`
/// leaf, only when it already exists, and logs what is being cleared and
/// why so the destructive step is never silent.
fn clear_snapcraft_cache(xdg_cache_home: Option<&str>, home: Option<&str>, log: &StageLogger) {
    let Some(cache) = snapcraft_download_cache(xdg_cache_home, home) else {
        return;
    };
    if !cache.exists() {
        return;
    }
    log.status(&format!(
        "snapcraft: clearing stale download cache at {} (works around snapcraft ≤8.14.5 import-time os.makedirs crash)",
        cache.display()
    ));
    let _ = std::fs::remove_dir_all(&cache);
}

// ---------------------------------------------------------------------------
// Icon copy helper
// ---------------------------------------------------------------------------

/// Resolve a user-configured icon path against the project root.
///
/// Absolute paths pass through unchanged. Relative paths are joined to
/// `project_root` when set, otherwise to the process CWD (`.`). Used by
/// both the early-validation site and the actual copy site so the two
/// can't drift on resolution rules.
fn resolve_icon_path(icon_src_str: &str, project_root: Option<&PathBuf>) -> PathBuf {
    if std::path::Path::new(icon_src_str).is_absolute() {
        PathBuf::from(icon_src_str)
    } else {
        project_root
            .map(|p| p.as_path())
            .unwrap_or(std::path::Path::new("."))
            .join(icon_src_str)
    }
}

/// Copy a snap icon source file into `<meta_dir>/gui/<snap_name>.<ext>`.
///
/// `icon_src`: the resolved path (absolute when `project_root` is set,
/// otherwise relative to the process CWD).
/// `meta_dir` is the `prime/meta/` directory that snapcraft reads from
/// when packing a pre-staged prime dir. `snap_name` is the snap's name
/// field — the destination filename matches the name so the Store's GUI
/// metadata channel picks up the correct icon.
///
/// Extension is preserved from the source so `.svg` icons round-trip
/// correctly alongside `.png`. The caller is expected to have already
/// validated the extension against snapcraft's allowed set.
pub(crate) fn copy_snap_icon(
    icon_src: &std::path::Path,
    meta_dir: &std::path::Path,
    snap_name: &str,
) -> anyhow::Result<String> {
    let ext = icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let gui_dir = meta_dir.join("gui");
    fs::create_dir_all(&gui_dir)
        .with_context(|| format!("snapcraft: create meta/gui dir: {}", gui_dir.display()))?;
    let dest_name = format!("{}.{}", snap_name, ext);
    let icon_dest = gui_dir.join(&dest_name);
    fs::copy(icon_src, &icon_dest).with_context(|| {
        format!(
            "snapcraft: copy icon {} to {}",
            icon_src.display(),
            icon_dest.display()
        )
    })?;
    Ok(format!("meta/gui/{}", dest_name))
}

// ---------------------------------------------------------------------------
// SnapcraftStage
// ---------------------------------------------------------------------------

pub struct SnapcraftStage;

/// A fully-staged snapcraft job ready for parallel `snapcraft pack`
/// invocation. Step 1 (serial, `&mut ctx`) stages the prime dir and
/// renders templates; Step 2 (parallel) runs the subprocess. `_tmp_dir`
/// keeps the staging dir alive through Step 2 — its `Drop` deletes the
/// directory when the job's worker thread finishes.
struct SnapcraftJob {
    _tmp_dir: tempfile::TempDir,
    snap_path: PathBuf,
    cmd_args: Vec<String>,
    target: Option<String>,
    crate_name: String,
    artifact_metadata: HashMap<String, String>,
}

impl Stage for SnapcraftStage {
    fn name(&self) -> &str {
        "snapcraft"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have snapcraft config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();
        let mut jobs: Vec<SnapcraftJob> = Vec::new();

        for krate in &crates {
            let Some(snap_configs) = krate.snapcrafts.as_ref() else {
                continue;
            };

            // Collect all Linux binary artifacts for this crate
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodizer_core::target::is_linux)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for snap_cfg in snap_configs {
                if validate_and_check_skip(ctx, &log, snap_cfg, &krate.name)? {
                    continue;
                }

                // Filter binaries by ids if configured (C2)
                let mut filtered_binaries = linux_binaries.clone();
                if let Some(ref filter_ids) = snap_cfg.ids
                    && !filter_ids.is_empty()
                {
                    filtered_binaries.retain(|b| {
                        b.metadata
                            .get("id")
                            .map(|id| filter_ids.contains(id))
                            .unwrap_or(false)
                            || b.metadata
                                .get("name")
                                .map(|n| filter_ids.contains(n))
                                .unwrap_or(false)
                    });
                }

                // Warn and skip if no linux binaries found
                if filtered_binaries.is_empty() && linux_binaries.is_empty() {
                    log.warn(&format!(
                        "no Linux binaries found for crate '{}'; skipping snapcraft",
                        krate.name
                    ));
                    continue;
                }
                if filtered_binaries.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        snap_cfg.ids, krate.name
                    ));
                    continue;
                }

                // Group binaries by target triple (platform) — one snap per
                // platform. `BTreeMap` (not `HashMap`) so iteration order is
                // deterministic across runs; this map is iterated below to
                // register one snap Artifact per target, and `HashMap`'s
                // randomised iteration would bake per-run order into
                // `dist/artifacts.json`. See the matching note in
                // `stage-archive/src/run.rs::run` for the harness regression
                // that prompted this.
                let mut by_target: BTreeMap<String, Vec<&Artifact>> = BTreeMap::new();
                for b in &filtered_binaries {
                    let target = b.target.clone().unwrap_or_else(|| "unknown".to_string());
                    by_target.entry(target).or_default().push(b);
                }

                for (target_key, target_binaries) in &by_target {
                    process_snap_target(
                        ctx,
                        &log,
                        snap_cfg,
                        &krate.name,
                        target_key,
                        target_binaries,
                        &dist,
                        &version,
                        dry_run,
                        &mut new_artifacts,
                        &mut archives_to_remove,
                        &mut jobs,
                    )?;
                }
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        if !jobs.is_empty() {
            let home_for_cache = ctx.env_var("HOME");
            let xdg_cache_home = ctx.env_var("XDG_CACHE_HOME");
            let packed = run_snap_jobs(
                &jobs,
                xdg_cache_home.as_deref(),
                home_for_cache.as_deref(),
                &log,
                parallelism,
            )?;
            new_artifacts.extend(packed);
        }

        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-target helper
// ---------------------------------------------------------------------------

/// Process one target platform for a snapcraft config: validate arch,
/// compute filename, and either emit dry-run output or stage the prime
/// directory and enqueue a `SnapcraftJob`.
#[allow(clippy::too_many_arguments)]
fn process_snap_target(
    ctx: &mut Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    crate_name: &str,
    target_key: &str,
    target_binaries: &[&Artifact],
    dist: &Path,
    version: &str,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    archives_to_remove: &mut Vec<PathBuf>,
    jobs: &mut Vec<SnapcraftJob>,
) -> Result<()> {
    let target = if target_key == "unknown" {
        None
    } else {
        Some(target_key.to_string())
    };

    if let Some(ref t) = target {
        let snap_arch = triple_to_snap_arch(t);
        if !is_valid_snap_arch(snap_arch) {
            log.warn(&format!(
                "snapcraft: skipping unsupported arch '{}' (target: {})",
                snap_arch, t
            ));
            return Ok(());
        }
    }

    let (os, arch) = target
        .as_deref()
        .map(anodizer_core::target::map_target)
        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

    let output_dir = dist.join("linux");
    if !dry_run {
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("create snapcraft output dir: {}", output_dir.display()))?;
    }

    let snap_name = snap_cfg.name.as_deref().unwrap_or(crate_name);
    let snap_filename = compute_snap_filename(
        ctx,
        snap_cfg,
        crate_name,
        snap_name,
        target.as_deref(),
        &os,
        &arch,
    )?;
    let snap_path = output_dir.join(&snap_filename);

    let artifact_metadata = {
        let mut m = HashMap::new();
        if let Some(id) = &snap_cfg.id {
            m.insert("id".to_string(), id.clone());
        }
        m
    };

    if dry_run {
        log.status(&format!(
            "(dry-run) would run: snapcraft pack --output {} for crate {} target {:?}",
            snap_path.display(),
            crate_name,
            target,
        ));
        new_artifacts.push(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: snap_path,
            target: target.clone(),
            crate_name: crate_name.to_string(),
            metadata: artifact_metadata,
            size: None,
        });
        archives_to_remove.extend(anodizer_core::util::collect_if_replace(
            snap_cfg.replace,
            &ctx.artifacts,
            crate_name,
            target.as_deref(),
        ));
        return Ok(());
    }

    let (tmp_dir, prime_dir) = stage_prime_dir(
        ctx,
        log,
        snap_cfg,
        crate_name,
        snap_name,
        target_binaries,
        target.as_deref(),
        version,
    )?;

    let cmd_args = snapcraft_command(&prime_dir.to_string_lossy(), &snap_path.to_string_lossy());

    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
        snap_cfg.replace,
        &ctx.artifacts,
        crate_name,
        target.as_deref(),
    ));

    jobs.push(SnapcraftJob {
        _tmp_dir: tmp_dir,
        snap_path,
        cmd_args,
        target: target.clone(),
        crate_name: crate_name.to_string(),
        artifact_metadata,
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate per-config fields and honour `skip:`. Returns `Ok(true)` when
/// the caller should `continue` to the next snap config (skip evaluated
/// true). Bails on invalid confinement / grade / icon settings.
fn validate_and_check_skip(
    ctx: &mut Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<bool> {
    if let Some(ref d) = snap_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("snapcraft: render skip template for crate {}", krate_name))?;
        if off {
            log.status(&format!(
                "skipping snapcraft config for crate {} (skip=true)",
                krate_name
            ));
            return Ok(true);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        snap_cfg.if_condition.as_deref(),
        &format!("snapcraft config for crate '{krate_name}'"),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipping snapcraft config for crate {krate_name} — `if` condition evaluated falsy"
        ));
        return Ok(true);
    }

    if let Some(conf) = &snap_cfg.confinement {
        match conf.as_str() {
            "strict" | "devmode" | "classic" => {}
            other => anyhow::bail!(
                "snapcraft: invalid confinement '{}' for crate '{}'. \
                 Valid values are: strict, devmode, classic",
                other,
                krate_name
            ),
        }
    }

    if let Some(grade) = &snap_cfg.grade {
        match grade.as_str() {
            "stable" | "devel" => {}
            other => anyhow::bail!(
                "snapcraft: invalid grade '{}' for crate '{}'. \
                 Valid values are: stable, devel",
                other,
                krate_name
            ),
        }
    }

    // Icon validation: when `icon` is set, check the source file exists
    // AND its extension is in snapcraft's allowed set (png/svg) before
    // staging binaries. snapcraft pack silently rejects other formats
    // at pack time, after the operator already burned minutes on the run.
    if let Some(ref icon_src_str) = snap_cfg.icon {
        let icon_src = resolve_icon_path(icon_src_str, ctx.options.project_root.as_ref());
        let ext_lower = icon_src
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        match ext_lower.as_deref() {
            Some("png") | Some("svg") => {}
            _ => {
                anyhow::bail!(
                    "snapcraft: icon '{}' configured for crate '{}' has \
                     unsupported extension (resolved to '{}'). Snapcraft \
                     only accepts .png or .svg snap icons; rename or \
                     convert the source file.",
                    icon_src_str,
                    krate_name,
                    icon_src.display()
                );
            }
        }
        if !icon_src.exists() {
            anyhow::bail!(
                "snapcraft: icon '{}' configured for crate '{}' does not exist \
                 (resolved to '{}'). Create the file or correct the path in \
                 the snapcrafts.icon config field.",
                icon_src_str,
                krate_name,
                icon_src.display()
            );
        }
    }

    Ok(false)
}

/// Render `snap_cfg.name_template` (or the default template)
/// with per-target `Os` / `Arch` / `Arm` / `Amd64` / `Mips` / `Target`
/// substitutions. Saves and restores `ProjectName` around the render so
/// subsequent stages observe the same template-var state.
fn compute_snap_filename(
    ctx: &mut Context,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
    snap_name: &str,
    target: Option<&str>,
    os: &str,
    arch: &str,
) -> Result<String> {
    // Default snap name template:
    //   {{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}{{ with .Mips }}_{{ . }}{{ end }}{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}
    let saved_project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();
    ctx.template_vars_mut().set("ProjectName", snap_name);
    ctx.template_vars_mut().set("Os", os);
    // For ARM targets, split Arch="arm" and Arm="6"/"7" so the default
    // template (concatenating `{{ .Arch }}v{{ .Arm }}`) produces "armv6"
    // rather than "armv6v6".
    if let Some(version) = arch.strip_prefix("armv") {
        ctx.template_vars_mut().set("Arch", "arm");
        ctx.template_vars_mut().set("Arm", version);
    } else {
        ctx.template_vars_mut().set("Arch", arch);
        ctx.template_vars_mut().set("Arm", "");
    }
    ctx.template_vars_mut()
        .set("Amd64", if arch == "amd64" { "v1" } else { "" });
    ctx.template_vars_mut().set("Mips", "");
    // `{{ .Target }}` is optional and consumed only by user-provided
    // `name_template:` values; empty signals a host-target build (no
    // triple) and Tera renders the interpolation as the empty string.
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    let tmpl = snap_cfg
        .name_template
        .as_deref()
        .unwrap_or(DEFAULT_SNAP_NAME_TEMPLATE);
    let render_result = ctx.render_template(tmpl).with_context(|| {
        format!(
            "snapcraft: render name_template for crate {} target {:?}",
            krate_name, target
        )
    });
    ctx.template_vars_mut()
        .set("ProjectName", &saved_project_name);
    let rendered = render_result?;
    Ok(if rendered.to_lowercase().ends_with(".snap") {
        rendered
    } else {
        format!("{rendered}.snap")
    })
}

/// Stage the snapcraft prime dir: write `snap.yaml`, copy icon /
/// binaries / extra files / templated extras / completers, and apply
/// `mod_timestamp`. Returns the owning `TempDir` (its `Drop` reaps the
/// staged tree once the worker finishes) and the `prime/` subdirectory.
#[allow(clippy::too_many_arguments)]
fn stage_prime_dir(
    ctx: &Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
    snap_name: &str,
    target_binaries: &[&Artifact],
    target: Option<&str>,
    version: &str,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let tmp_dir = tempfile::tempdir().context("create temp dir for snapcraft build")?;
    let prime_dir = tmp_dir.path().join("prime");
    let meta_dir = prime_dir.join("meta");
    fs::create_dir_all(&meta_dir)
        .with_context(|| format!("create prime/meta dir: {}", meta_dir.display()))?;

    let all_binary_names: Vec<String> = target_binaries
        .iter()
        .map(|b| {
            b.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("binary")
                .to_string()
        })
        .collect();
    let binary_name_refs: Vec<&str> = all_binary_names.iter().map(|s| s.as_str()).collect();

    let rendered_cfg = render_snap_cfg(ctx, snap_cfg, krate_name)?;

    // Generate and write snap.yaml to prime/meta/snap.yaml
    let project_name = &ctx.config.project_name;
    let yaml_content = generate_snap_yaml(
        &rendered_cfg,
        version,
        &binary_name_refs,
        target,
        Some(project_name.as_str()),
    )?;
    let yaml_path = meta_dir.join("snap.yaml");
    fs::write(&yaml_path, &yaml_content)
        .with_context(|| format!("write snap.yaml to {}", yaml_path.display()))?;

    // Copy icon into meta/gui/ so snapcraft picks it up via the GUI
    // metadata channel without touching snap.yaml. The Snap Store
    // rejects snap.json with an `icon:` key, so the field is
    // intentionally omitted from snap.yaml (see generate_snap_yaml).
    if let Some(ref icon_src_str) = snap_cfg.icon {
        let icon_src = resolve_icon_path(icon_src_str, ctx.options.project_root.as_ref());
        let dest_rel = copy_snap_icon(&icon_src, &meta_dir, snap_name)?;
        log.status(&format!("snapcraft: wrote snap icon to {}", dest_rel));
    }

    copy_binaries_into_prime(target_binaries, &prime_dir)?;
    copy_extra_files(snap_cfg, &prime_dir)?;
    copy_completer_files(ctx, snap_cfg, &prime_dir)?;

    if let Some(ref tpl_specs) = snap_cfg.templated_extra_files
        && !tpl_specs.is_empty()
    {
        anodizer_core::templated_files::process_templated_extra_files(
            tpl_specs,
            ctx,
            &prime_dir,
            "snapcraft",
        )?;
    }

    if let Some(ts) = &snap_cfg.mod_timestamp {
        anodizer_core::util::apply_mod_timestamp(&prime_dir, ts, log)?;
    }

    Ok((tmp_dir, prime_dir))
}

/// Clone `snap_cfg` and pre-render its summary / description / grade
/// fields through the template engine. Fall back
/// to project `metadata.description` when snapcraft's `description` is
/// unset.
fn render_snap_cfg(
    ctx: &Context,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<SnapcraftConfig> {
    let mut rendered_cfg = snap_cfg.clone();
    if rendered_cfg.description.is_none() {
        rendered_cfg.description = ctx
            .config
            .meta_description_for(krate_name)
            .map(str::to_string);
    }
    // `summary` is a snapcraft-required short tagline with no Cargo.toml
    // counterpart. Fall back to the (possibly Cargo.toml-derived)
    // description so a plain Rust project that declares only
    // `package.description` does not hard-error on "summary is required".
    if rendered_cfg.summary.is_none() {
        rendered_cfg.summary = rendered_cfg.description.clone();
    }
    if let Some(ref s) = rendered_cfg.summary {
        let rendered = ctx
            .render_template(s)
            .with_context(|| format!("snapcraft: render summary for crate {}", krate_name))?;
        rendered_cfg.summary = Some(truncate_snap_summary(&rendered));
    }
    if let Some(ref d) = rendered_cfg.description {
        rendered_cfg.description =
            Some(ctx.render_template(d).with_context(|| {
                format!("snapcraft: render description for crate {}", krate_name)
            })?);
    }
    if let Some(ref g) = rendered_cfg.grade {
        rendered_cfg.grade = Some(
            ctx.render_template(g)
                .with_context(|| format!("snapcraft: render grade for crate {}", krate_name))?,
        );
    }
    Ok(rendered_cfg)
}

/// snapcraft's `summary` is hard-capped at 78 characters; a longer value
/// fails at `snapcraft pack`. Deriving the summary from an arbitrarily long
/// `package.description` (or a user-supplied over-long summary) can exceed
/// it, so truncate to 78 characters here — the single point where the
/// effective summary is finalised — applying the cap to derived and
/// user-set summaries alike.
fn truncate_snap_summary(summary: &str) -> String {
    const MAX_SUMMARY_CHARS: usize = 78;
    if summary.chars().count() <= MAX_SUMMARY_CHARS {
        return summary.to_string();
    }
    summary.chars().take(MAX_SUMMARY_CHARS).collect()
}

/// Copy binaries into the prime dir root with mode 0555.
fn copy_binaries_into_prime(target_binaries: &[&Artifact], prime_dir: &Path) -> Result<()> {
    for bin_artifact in target_binaries {
        let bin_name = bin_artifact
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("binary");
        let binary_dest = prime_dir.join(bin_name);
        let bin_path_str = bin_artifact.path.to_string_lossy();
        fs::copy(&bin_artifact.path, &binary_dest).with_context(|| {
            format!("copy binary {} to {}", bin_path_str, binary_dest.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o555);
            std::fs::set_permissions(&binary_dest, perms)
                .with_context(|| format!("set binary mode 0555 on {}", binary_dest.display()))?;
        }
    }
    Ok(())
}

/// Copy each entry of `extra_files` into the prime dir at its
/// destination path, applying the configured file mode (default 0644).
fn copy_extra_files(snap_cfg: &SnapcraftConfig, prime_dir: &Path) -> Result<()> {
    let Some(extra_files) = &snap_cfg.extra_files else {
        return Ok(());
    };
    for extra in extra_files {
        let src = PathBuf::from(extra.source());
        let dest_rel = extra.destination().unwrap_or_else(|| extra.source());
        let dest = prime_dir.join(dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir for extra file: {}", parent.display()))?;
        }
        fs::copy(&src, &dest)
            .with_context(|| format!("copy extra file {} to {}", src.display(), dest.display()))?;
        let mode = extra.mode().unwrap_or(0o644);
        if mode > 0o7777 {
            anyhow::bail!(
                "snapcraft: invalid file mode {:o} for '{}' — \
                 must be in range 0-7777 (octal)",
                mode,
                src.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&dest, perms)
                .with_context(|| format!("set mode {:o} on {}", mode, dest.display()))?;
        }
    }
    Ok(())
}

/// Copy per-app completer scripts into the prime dir. The `completer:`
/// path is used twice (source AND destination) — an absolute value
/// collapses the two because `Path::join(absolute)` discards the prefix
/// on every platform, so reject absolute paths at the contract boundary.
fn copy_completer_files(ctx: &Context, snap_cfg: &SnapcraftConfig, prime_dir: &Path) -> Result<()> {
    let Some(ref apps_map) = snap_cfg.apps else {
        return Ok(());
    };
    for (app_name, app_cfg) in apps_map.iter() {
        let Some(ref completer_path) = app_cfg.completer else {
            continue;
        };
        if Path::new(completer_path).is_absolute() {
            anyhow::bail!(
                "snapcraft: app '{}' completer path '{}' must be \
                 relative to the project root (the same path is also \
                 used as the destination inside the snap's prime dir; \
                 absolute paths collapse source and destination)",
                app_name,
                completer_path,
            );
        }
        let src = ctx
            .options
            .project_root
            .as_deref()
            .unwrap_or(Path::new("."))
            .join(completer_path);
        let dest = prime_dir.join(completer_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("snapcraft: create dir for completer {}", parent.display())
            })?;
        }
        if src.exists() {
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "snapcraft: copy completer {} to {}",
                    src.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Run all staged `snapcraft pack` jobs with bounded parallelism.
/// Serializes the cache-wipe + pack pair across workers via
/// `SNAPCRAFT_CACHE_LOCK` so each invocation sees a non-existent cache
/// dir at import time.
fn run_snap_jobs(
    jobs: &[SnapcraftJob],
    xdg_cache_home: Option<&str>,
    home_for_cache: Option<&str>,
    log: &StageLogger,
    parallelism: usize,
) -> Result<Vec<Artifact>> {
    let run_job = |job: &SnapcraftJob| -> Result<Artifact> {
        let thread_log = StageLogger::new("snapcraft", log.verbosity());

        let _cache_guard = SNAPCRAFT_CACHE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("snapcraft cache lock poisoned"))?;
        clear_snapcraft_cache(xdg_cache_home, home_for_cache, &thread_log);

        thread_log.status(&format!("running: {}", job.cmd_args.join(" ")));

        let output = Command::new(&job.cmd_args[0])
            .args(&job.cmd_args[1..])
            .output()
            .with_context(|| {
                format!(
                    "execute snapcraft for crate {} target {:?}",
                    job.crate_name, job.target
                )
            })?;
        thread_log.check_output(output, "snapcraft pack")?;

        Ok(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: job.snap_path.clone(),
            target: job.target.clone(),
            crate_name: job.crate_name.clone(),
            metadata: job.artifact_metadata.clone(),
            size: None,
        })
    };

    anodizer_core::parallel::run_parallel_chunks(jobs, parallelism, "snapcraft", run_job)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use anodizer_core::log::Verbosity;

    #[test]
    fn xdg_cache_home_wins_over_home() {
        let resolved = snapcraft_download_cache(Some("/xdg/cache"), Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/xdg/cache/snapcraft/download"))
        );
    }

    #[test]
    fn falls_back_to_home_dot_cache() {
        let resolved = snapcraft_download_cache(None, Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/home/u/.cache/snapcraft/download"))
        );
    }

    #[test]
    fn empty_xdg_falls_back_to_home() {
        let resolved = snapcraft_download_cache(Some(""), Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/home/u/.cache/snapcraft/download"))
        );
    }

    #[test]
    fn no_home_and_no_xdg_resolves_nothing() {
        assert_eq!(snapcraft_download_cache(None, None), None);
        assert_eq!(snapcraft_download_cache(Some(""), Some("")), None);
    }

    #[test]
    fn target_is_the_download_leaf_never_a_parent() {
        let resolved = snapcraft_download_cache(None, Some("/home/u")).unwrap();
        assert!(resolved.ends_with("snapcraft/download"));
    }

    #[test]
    fn clear_only_removes_existing_download_subdir_not_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path();
        let download = xdg.join("snapcraft").join("download");
        std::fs::create_dir_all(&download).unwrap();
        // A sibling under the snapcraft cache that must survive the wipe.
        let sibling = xdg.join("snapcraft").join("keepme");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(download.join("blob.bin"), b"stale").unwrap();

        let log = StageLogger::new("snapcraft-cache-test", Verbosity::Quiet);
        clear_snapcraft_cache(Some(xdg.to_str().unwrap()), None, &log);

        assert!(!download.exists(), "download leaf must be wiped");
        assert!(
            sibling.exists(),
            "wipe must not touch the snapcraft cache parent or its siblings"
        );
    }

    #[test]
    fn clear_is_a_noop_when_cache_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("does-not-exist");
        let log = StageLogger::new("snapcraft-cache-test", Verbosity::Quiet);
        // Must not error or create anything.
        clear_snapcraft_cache(Some(xdg.to_str().unwrap()), None, &log);
        assert!(!xdg.join("snapcraft").exists());
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    /// Build a Context whose single crate's `Cargo.toml [package].description`
    /// supplies derived metadata, with NO top-level `metadata:` block.
    fn ctx_with_cargo_description(description: &str) -> (Context, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("demo");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\ndescription = \"{description}\"\n"),
        )
        .unwrap();
        let mut ctx = TestContextBuilder::new().build();
        assert!(ctx.config.metadata.is_none(), "no metadata: block present");
        ctx.config.crates = vec![CrateConfig {
            name: "demo".to_string(),
            path: "demo".to_string(),
            ..Default::default()
        }];
        ctx.config.populate_derived_metadata(tmp.path());
        (ctx, tmp)
    }

    #[test]
    fn summary_resolves_from_cargo_toml_description() {
        // Previously: snapcraft "summary is required" — now the summary falls
        // back to the Cargo.toml description.
        let (ctx, _tmp) = ctx_with_cargo_description("a concise demo summary");
        let snap_cfg = SnapcraftConfig::default();
        assert!(snap_cfg.summary.is_none());

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.summary.as_deref(), Some("a concise demo summary"));
    }

    #[test]
    fn derived_over_long_summary_is_capped_at_78_chars() {
        // A >78-char description must not produce a summary that fails at
        // `snapcraft pack`; the cap applies to the derived summary.
        let long = "x".repeat(120);
        let (ctx, _tmp) = ctx_with_cargo_description(&long);
        let snap_cfg = SnapcraftConfig::default();

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").unwrap();
        let summary = rendered.summary.expect("summary derived");
        assert_eq!(
            summary.chars().count(),
            78,
            "derived summary must be capped at 78 chars; got {} chars",
            summary.chars().count()
        );
    }

    #[test]
    fn user_set_over_long_summary_is_capped_at_78_chars() {
        // The cap is applied consistently to a user-supplied over-long summary.
        let mut ctx = TestContextBuilder::new().build();
        ctx.config = Config::default();
        let snap_cfg = SnapcraftConfig {
            summary: Some("y".repeat(100)),
            ..Default::default()
        };
        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").unwrap();
        let summary = rendered.summary.expect("summary present");
        assert!(
            summary.chars().count() <= 78,
            "user-set summary must be capped at <= 78 chars; got {} chars",
            summary.chars().count()
        );
    }

    #[test]
    fn short_summary_is_left_unchanged() {
        let summary = truncate_snap_summary("short and fine");
        assert_eq!(summary, "short and fine");
    }
}
