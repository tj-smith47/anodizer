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
use anodizer_core::template::assert_no_unrendered_logged;

use crate::arch::{is_valid_snap_arch, triple_to_snap_arch};
use crate::command::{first_channel_rejected_for_prerelease_snap, snapcraft_command};
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
/// leaf, only when it already exists, and logs (at verbose) what is being
/// cleared and why so the destructive step is auditable under `-v`.
fn clear_snapcraft_cache(xdg_cache_home: Option<&str>, home: Option<&str>, log: &StageLogger) {
    let Some(cache) = snapcraft_download_cache(xdg_cache_home, home) else {
        return;
    };
    if !cache.exists() {
        return;
    }
    log.verbose(&format!(
        "clearing stale download cache at {} (works around snapcraft ≤8.14.5 import-time os.makedirs crash)",
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
            .crate_universe()
            .into_iter()
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

            // Collect all Linux binary artifacts for this crate, cloned so the
            // mutable `process_snap_target` borrow below does not conflict with
            // the artifact-registry borrow.
            let linux_binaries: Vec<Artifact> = linux_binaries_for_crate(ctx, &krate.name)
                .into_iter()
                .cloned()
                .collect();

            // One guard per crate spans every `snapcrafts:` config of that crate:
            // two configs with the default (or identical) `name:` render the same
            // `.snap` path for one arch — error loudly across configs instead of
            // letting the second silently clobber the first.
            let mut name_guard = anodizer_core::arch_path_guard::ArchPathGuard::new();

            for snap_cfg in snap_configs {
                if validate_and_check_skip(ctx, &log, snap_cfg, &krate.name)? {
                    continue;
                }

                let linux_refs: Vec<&Artifact> = linux_binaries.iter().collect();
                let filtered_binaries = filter_binaries_by_ids(&linux_refs, snap_cfg.ids.as_ref());

                // Warn and skip if no linux binaries found
                if filtered_binaries.is_empty() && linux_binaries.is_empty() {
                    log.warn(&format!(
                        "skipped snapcraft for crate '{}' — no Linux binaries found",
                        krate.name
                    ));
                    continue;
                }
                if filtered_binaries.is_empty() {
                    log.warn(&format!(
                        "skipped snapcraft for crate '{}' — ids filter {:?} matched no binaries",
                        krate.name, snap_cfg.ids
                    ));
                    continue;
                }

                let by_target = group_binaries_by_target(&filtered_binaries);

                for ((target_key, amd64_variant), target_binaries) in &by_target {
                    process_snap_target(
                        ctx,
                        &log,
                        snap_cfg,
                        &krate.name,
                        target_key,
                        amd64_variant.as_deref(),
                        target_binaries,
                        &dist,
                        &version,
                        dry_run,
                        &mut new_artifacts,
                        &mut archives_to_remove,
                        &mut jobs,
                        &mut name_guard,
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

/// Group key for one snap: target triple plus the amd64 micro-architecture
/// variant. Two amd64 builds of one triple (baseline `v1` and, e.g., `v3`)
/// share `Os`/`Arch` but must produce distinct snaps, so the variant is part
/// of the grouping key.
type SnapTargetKey = (String, Option<String>);

/// Group a crate's Linux binary artifacts by `(target triple, amd64_variant)`
/// — one snap per platform-variant. `BTreeMap` (not `HashMap`) so iteration
/// order is deterministic across runs; the map is iterated to register one
/// snap Artifact per key, and `HashMap`'s randomised iteration would bake
/// per-run order into `dist/artifacts.json`. A binary with no target lands
/// under the `unknown` key (a host-target build with no triple). Both the
/// build's `run` loop and the offline `snapcraft_snap_yamls_for_crate`
/// renderer call this so the two can never diverge on grouping.
fn group_binaries_by_target<'a>(
    binaries: &[&'a Artifact],
) -> BTreeMap<SnapTargetKey, Vec<&'a Artifact>> {
    let mut by_target: BTreeMap<SnapTargetKey, Vec<&Artifact>> = BTreeMap::new();
    for b in binaries {
        let target = b.target.clone().unwrap_or_else(|| "unknown".to_string());
        let variant = b.metadata.get("amd64_variant").cloned();
        by_target.entry((target, variant)).or_default().push(b);
    }
    by_target
}

/// Collect a crate's Linux binary artifacts in artifact-registry order.
///
/// Both the build loop and the offline renderer start from this exact set
/// before applying the per-config `ids` filter, so the validated universe
/// equals the published universe.
fn linux_binaries_for_crate<'a>(ctx: &'a Context, crate_name: &str) -> Vec<&'a Artifact> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_linux)
                .unwrap_or(false)
        })
        .collect()
}

/// Apply a snap config's `ids` allow-list to a crate's Linux binaries.
///
/// An empty / absent `ids` admits every binary. A non-empty `ids` keeps only
/// binaries whose `id` or `name` metadata matches. Shared by the build loop
/// and the offline renderer so both honour identical eligibility.
fn filter_binaries_by_ids<'a>(
    binaries: &[&'a Artifact],
    ids: Option<&Vec<String>>,
) -> Vec<&'a Artifact> {
    let mut filtered: Vec<&Artifact> = binaries.to_vec();
    if let Some(filter_ids) = ids
        && !filter_ids.is_empty()
    {
        filtered.retain(|b| {
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
    filtered
}

/// Render the snap.yaml metadata a build would write to
/// `prime/meta/snap.yaml` for one snap config on one target.
///
/// Renders the config's templated fields (summary / description / grade,
/// with the project-description fallback and the 78-char summary cap) and
/// hands them to [`generate_snap_yaml`]. This is the single source of truth
/// the build's prime-dir staging and the offline schema validator both call,
/// so a validated document is byte-for-byte the metadata a release ships.
///
/// `binary_names` are the binary filenames staged into the prime root (the
/// first names the default app when no `apps:` are configured); `target` is
/// the optional triple driving the `architectures:` field.
pub(crate) fn render_snap_yaml(
    ctx: &Context,
    snap_cfg: &SnapcraftConfig,
    crate_name: &str,
    version: &str,
    binary_names: &[&str],
    target: Option<&str>,
    project_name: Option<&str>,
) -> Result<String> {
    let rendered_cfg = render_snap_cfg(ctx, snap_cfg, crate_name)?;
    let yaml = generate_snap_yaml(&rendered_cfg, version, binary_names, target, project_name)?;
    // Final chokepoint: catch any user-supplied field that reached the manifest
    // without template rendering. Strict fails the build before publish; lenient
    // warns with the residual already redacted.
    let log = ctx.logger("snapcraft");
    assert_no_unrendered_logged(
        &yaml,
        "snapcraft.yaml",
        ctx.render_is_strict(),
        |s| ctx.redact(s),
        |msg| log.warn(msg),
    )?;
    Ok(yaml)
}

/// Render every snap.yaml a build would emit for one crate, mirroring the
/// build's per-target run walk — without staging files or spawning snapcraft.
///
/// Returns `Ok(vec![])` (nothing to validate) when the crate carries no
/// snapcraft config, when a config's `skip:` / `if:` gate suppresses it, or
/// when no Linux binaries were built for the crate in this snapshot shard
/// (the same shard-tolerance case the build's "no Linux binaries → skip"
/// guard hits). Otherwise groups the crate's Linux binaries by target via the
/// same helpers the build loop uses and returns one rendered snap.yaml per
/// (config, target) pair, each stamped with the run's resolved version.
pub fn snapcraft_snap_yamls_for_crate(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    let log = ctx.logger("snapcraft");
    let Some(krate) = ctx.config.find_crate(crate_name) else {
        return Ok(Vec::new());
    };
    let Some(snap_configs) = krate.snapcrafts.as_ref() else {
        return Ok(Vec::new());
    };

    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string());
    let project_name = ctx.config.project_name.clone();

    let linux_binaries = linux_binaries_for_crate(ctx, crate_name);

    let mut yamls = Vec::new();
    for snap_cfg in snap_configs {
        if snap_cfg_skipped(ctx, &log, snap_cfg, crate_name)? {
            continue;
        }

        let filtered = filter_binaries_by_ids(&linux_binaries, snap_cfg.ids.as_ref());
        // No Linux binary for this crate in this shard (or the `ids` filter
        // admitted none) — nothing to render. The live build's
        // "no Linux binaries → skip" guard hits the same case.
        if filtered.is_empty() {
            continue;
        }

        let by_target = group_binaries_by_target(&filtered);
        // Grouping keys on the variant to match the live build's per-variant
        // snap builds 1:1; the variant disambiguates only the `.snap` FILENAME
        // (`compute_snap_filename`), not the manifest — the snap `name:` is the
        // package name — so the rendered YAML is variant-independent here.
        for ((target_key, _amd64_variant), target_binaries) in &by_target {
            let target = if target_key == "unknown" {
                None
            } else {
                Some(target_key.as_str())
            };

            // Mirror the build's per-target arch gate: `process_snap_target`
            // refuses to stage a target whose snap arch is unsupported by the
            // store (e.g. riscv64). Skip the same targets here so the validated
            // (target → snap.yaml) set is byte-identical to the built set.
            if let Some(t) = target
                && !is_valid_snap_arch(triple_to_snap_arch(t))
            {
                continue;
            }

            let binary_names: Vec<String> = target_binaries
                .iter()
                .map(|b| {
                    b.path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("binary")
                        .to_string()
                })
                .collect();
            let binary_name_refs: Vec<&str> = binary_names.iter().map(|s| s.as_str()).collect();

            yamls.push(render_snap_yaml(
                ctx,
                snap_cfg,
                crate_name,
                &version,
                &binary_name_refs,
                target,
                Some(project_name.as_str()),
            )?);
        }
    }

    Ok(yamls)
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
    amd64_variant: Option<&str>,
    target_binaries: &[&Artifact],
    dist: &Path,
    version: &str,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    archives_to_remove: &mut Vec<PathBuf>,
    jobs: &mut Vec<SnapcraftJob>,
    name_guard: &mut anodizer_core::arch_path_guard::ArchPathGuard,
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
                "skipped arch '{}' — unsupported (target: {})",
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
        amd64_variant,
    )?;
    let snap_path = output_dir.join(&snap_filename);
    let name_template = snap_cfg
        .name_template
        .as_deref()
        .unwrap_or(DEFAULT_SNAP_NAME_TEMPLATE);
    name_guard.check(
        &snap_path,
        "snapcrafts",
        "snap",
        name_template,
        &snap_filename,
        crate_name,
    )?;

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

/// Evaluate a snap config's `skip:` / `if:` gates against the render context.
///
/// Returns `Ok(true)` when the config is suppressed — `skip:` rendered truthy
/// or the `if:` condition rendered falsy — so the caller skips it. Read-only
/// (`&Context`), so both the build's `validate_and_check_skip` and the offline
/// `snapcraft_snap_yamls_for_crate` renderer share one gate and never diverge
/// on which configs a run suppresses.
fn snap_cfg_skipped(
    ctx: &Context,
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
                "skipped snapcraft config for crate {} — skip=true",
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
            "skipped snapcraft config for crate {krate_name} — `if` condition evaluated falsy"
        ));
        return Ok(true);
    }
    Ok(false)
}

/// Validate per-config fields and honour `skip:`. Returns `Ok(true)` when
/// the caller should `continue` to the next snap config (skip evaluated
/// true). Bails on invalid confinement / grade / icon settings.
fn validate_and_check_skip(
    ctx: &mut Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<bool> {
    if snap_cfg_skipped(ctx, log, snap_cfg, krate_name)? {
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

    // Confinement/grade vs. channel cross-check: the Snap Store rejects a
    // devmode-confined or devel-grade snap ("not ready for general use")
    // pushed to candidate/stable. Catch this at preflight, before any
    // build/upload work, rather than surfacing it as an upload-time Store
    // rejection. An unset `channel_templates` auto-populates to edge/beta
    // for these snaps (see `resolve_effective_channels`) and never reaches
    // this branch — only an explicit, conflicting channel is bailed on.
    //
    // This check runs against the RAW, unrendered `channel_templates` and
    // `grade` strings, and only when the build stage itself executes — a
    // template that resolves to a restricted channel only after rendering,
    // or a `--publish-only` run (which skips the build stage entirely),
    // never reaches it. `run_uploads` in `publish_stage.rs` re-runs the same
    // classifier against the RENDERED values immediately before every
    // upload, which is the only check both paths always hit.
    let confinement_is_devmode = snap_cfg.confinement.as_deref() == Some("devmode");
    let grade_is_devel = snap_cfg.grade.as_deref() == Some("devel");
    if confinement_is_devmode || grade_is_devel {
        if let Some(channels) = snap_cfg.channel_templates.as_deref() {
            if let Some(rejected) = first_channel_rejected_for_prerelease_snap(channels) {
                let reason = match (confinement_is_devmode, grade_is_devel) {
                    (true, true) => "devmode confinement and devel grade",
                    (true, false) => "devmode confinement",
                    (false, true) => "devel grade",
                    (false, false) => unreachable!("guarded by the outer if"),
                };
                anyhow::bail!(
                    "snapcraft: crate '{krate_name}' configures {reason} together \
                     with channel '{rejected}', which the Snap Store rejects — a \
                     snap with {reason} may only be pushed to pre-release channels \
                     (edge, beta). Remove '{rejected}' from channel_templates or \
                     drop the setting that produces {reason}."
                );
            }
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
#[allow(clippy::too_many_arguments)]
fn compute_snap_filename(
    ctx: &mut Context,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
    snap_name: &str,
    target: Option<&str>,
    os: &str,
    arch: &str,
    amd64_variant: Option<&str>,
) -> Result<String> {
    let saved_project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();
    ctx.template_vars_mut().set("ProjectName", snap_name);
    match target {
        // The archive-name seeding policy verbatim (arm split, variant vars
        // empty) — the snap default IS core's default asset-name template, so
        // the vars it reads must be seeded identically.
        Some(t) => anodizer_core::archive_name::seed_target_vars(ctx, t),
        // Host-target build (no triple): seed the caller-derived Os/Arch and
        // clear ALL variant vars — resetting a subset would leak a previous
        // target's `Arm64`/`I386` into a user name_template. An empty
        // `{{ .Target }}` renders as the empty string in user templates.
        None => {
            ctx.template_vars_mut().set("Os", os);
            ctx.template_vars_mut().set("Arch", arch);
            ctx.template_vars_mut().set("Target", "");
            anodizer_core::archive_name::reset_variant_vars(ctx.template_vars_mut());
        }
    }
    // The amd64 micro-architecture variant comes from the built binary's
    // metadata, not the go-arch. The default template's Amd64 clause
    // suppresses the `v1` baseline so `None`/`"v1"` preserve historical
    // single-variant snap names, while a non-`v1` variant (e.g. `"v3"`)
    // appends the suffix.
    anodizer_core::archive_name::seed_amd64_variant_var(
        ctx.template_vars_mut(),
        arch,
        amd64_variant,
    );
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

    // Generate and write snap.yaml to prime/meta/snap.yaml via the shared
    // render path the offline schema validator also calls, so the staged
    // metadata is byte-identical to what validation checks.
    let project_name = &ctx.config.project_name;
    let yaml_content = render_snap_yaml(
        ctx,
        snap_cfg,
        krate_name,
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
        log.status(&format!("wrote snap icon to {}", dest_rel));
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
    // The remaining user-supplied string fields are templatable too (GoReleaser
    // templates these); without rendering, a value like `title: "{{ .Tag }}"`
    // would ship the literal delimiters into snap.yaml.
    if let Some(ref n) = rendered_cfg.name {
        rendered_cfg.name = Some(
            ctx.render_template(n)
                .with_context(|| format!("snapcraft: render name for crate {}", krate_name))?,
        );
    }
    if let Some(ref b) = rendered_cfg.base {
        rendered_cfg.base = Some(
            ctx.render_template(b)
                .with_context(|| format!("snapcraft: render base for crate {}", krate_name))?,
        );
    }
    if let Some(ref c) = rendered_cfg.confinement {
        rendered_cfg.confinement =
            Some(ctx.render_template(c).with_context(|| {
                format!("snapcraft: render confinement for crate {}", krate_name)
            })?);
    }
    // Derive the SPDX license from the crate's Cargo.toml when the config
    // omits it, mirroring every other publisher's `meta_license_for` fallback
    // so a dual-licensed project does not have to hardcode it.
    if rendered_cfg.license.is_none() {
        rendered_cfg.license = ctx.config.meta_license_for(krate_name).map(str::to_string);
    }
    if let Some(ref l) = rendered_cfg.license {
        rendered_cfg.license = Some(
            ctx.render_template(l)
                .with_context(|| format!("snapcraft: render license for crate {}", krate_name))?,
        );
    }
    if let Some(ref t) = rendered_cfg.title {
        rendered_cfg.title = Some(
            ctx.render_template(t)
                .with_context(|| format!("snapcraft: render title for crate {}", krate_name))?,
        );
    }
    // App `command`/`args` are user-templatable (GoReleaser renders them, e.g.
    // `command: myapp-{{ .Version }}`); without rendering, the literal
    // delimiters would ship into snap.yaml — caught by the residual-delimiter
    // guard at the YAML chokepoint, so failing to render here is a hard error
    // under strict mode.
    if let Some(apps) = rendered_cfg.apps.as_mut() {
        for (app_name, app) in apps.iter_mut() {
            if let Some(ref c) = app.command {
                app.command = Some(ctx.render_template(c).with_context(|| {
                    format!("snapcraft: render app '{app_name}' command for crate {krate_name}")
                })?);
            }
            if let Some(ref a) = app.args {
                app.args = Some(ctx.render_template(a).with_context(|| {
                    format!("snapcraft: render app '{app_name}' args for crate {krate_name}")
                })?);
            }
        }
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

        thread_log.verbose(&format!("running {}", job.cmd_args.join(" ")));

        let mut command = Command::new(&job.cmd_args[0]);
        command.args(&job.cmd_args[1..]);
        anodizer_core::run::run_checked(&mut command, &thread_log, "snapcraft pack")?;

        let snap_name = job
            .snap_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.snap_path.display().to_string());
        thread_log.status(&format!("packed snap {snap_name}"));

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

    anodizer_core::parallel::run_parallel_chunks(jobs, parallelism, "snapcraft", log, run_job)
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

    fn ctx_with_cargo_license(license: &str) -> (Context, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("demo");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\nlicense = \"{license}\"\n"),
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
    fn license_resolves_from_cargo_toml_when_config_omits_it() {
        // snapcraft's `license` must derive from the crate's Cargo.toml SPDX
        // license (like every other publisher) when the config omits it, so a
        // dual-licensed project does not need to hardcode it.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig::default();
        assert!(snap_cfg.license.is_none());

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.license.as_deref(), Some("MIT OR Apache-2.0"));
    }

    #[test]
    fn emitted_snap_yaml_carries_derived_license() {
        // End-to-end: resolve the config (derive license from Cargo.toml) then
        // generate the snap.yaml, proving the emitted manifest — not just the
        // intermediate struct — carries `license: MIT OR Apache-2.0`.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig {
            summary: Some("a demo".to_string()),
            description: Some("a demo description".to_string()),
            ..Default::default()
        };
        assert!(snap_cfg.license.is_none());
        let resolved = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        let yaml = generate_snap_yaml(
            &resolved,
            "0.9.1",
            &["demo"],
            Some("x86_64-unknown-linux-gnu"),
            Some("demo"),
        )
        .expect("generate snap.yaml");
        assert!(
            yaml.contains("license: MIT OR Apache-2.0"),
            "emitted snap.yaml must carry the derived license: {yaml}"
        );
    }

    #[test]
    fn explicit_license_wins_over_derived() {
        // An explicit config license overrides the Cargo.toml-derived value.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig {
            license: Some("GPL-3.0".to_string()),
            ..Default::default()
        };
        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.license.as_deref(), Some("GPL-3.0"));
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

#[cfg(test)]
mod id_binding_tests {
    use super::*;
    use std::collections::HashMap;

    fn bin(path: &str, target: &str, id: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(path),
            target: Some(target.to_string()),
            crate_name: "anodizer".to_string(),
            metadata: HashMap::from([("id".to_string(), id.to_string())]),
            size: None,
        }
    }

    fn bin_variant(path: &str, target: &str, variant: Option<&str>) -> Artifact {
        let mut metadata = HashMap::new();
        if let Some(v) = variant {
            metadata.insert("amd64_variant".to_string(), v.to_string());
        }
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(path),
            target: Some(target.to_string()),
            crate_name: "anodizer".to_string(),
            metadata,
            size: None,
        }
    }

    #[test]
    fn ids_filter_binds_gnu_build_excludes_musl() {
        // The snap must ship the glibc binary. snapcraft groups by Os/Arch and
        // map_target collapses gnu and musl x86_64 to linux/amd64, so without
        // an `ids:` bind the musl binary renders the same linux_amd64 snap name
        // and clobbers (last-writer-wins). `ids: [anodizer]` must keep ONLY the
        // gnu build, never the musl one.
        let gnu = bin("dist/anodizer-gnu", "x86_64-unknown-linux-gnu", "anodizer");
        let musl = bin(
            "dist/anodizer-musl",
            "x86_64-unknown-linux-musl",
            "anodizer-musl",
        );
        let all = vec![&gnu, &musl];

        let ids = vec!["anodizer".to_string()];
        let filtered = filter_binaries_by_ids(&all, Some(&ids));

        assert_eq!(filtered.len(), 1, "exactly the gnu build survives the bind");
        let t = filtered[0].target.as_deref().unwrap_or("");
        assert!(
            t.contains("-linux-gnu"),
            "bound build must be the gnu binary, got target {t:?}"
        );
        assert!(
            !filtered
                .iter()
                .any(|b| b.target.as_deref().unwrap_or("").contains("-linux-musl")),
            "the musl binary must never reach the glibc snap"
        );
    }

    #[test]
    fn no_ids_auto_collects_both_builds_the_collision_we_guard_against() {
        // Documents the auto-collect hazard the bind fixes: with `ids` unset
        // BOTH x86_64 builds are admitted, and since they share os/arch the
        // snap filename collides. This is exactly why the config sets
        // `ids: [anodizer]` on `snapcrafts:`.
        let gnu = bin("dist/anodizer-gnu", "x86_64-unknown-linux-gnu", "anodizer");
        let musl = bin(
            "dist/anodizer-musl",
            "x86_64-unknown-linux-musl",
            "anodizer-musl",
        );
        let all = vec![&gnu, &musl];

        let filtered = filter_binaries_by_ids(&all, None);
        assert_eq!(filtered.len(), 2, "auto-collect admits both (the hazard)");
    }

    #[test]
    fn grouping_keys_on_target_and_amd64_variant() {
        // Two amd64 builds of one triple (baseline + v3) share Os/Arch but must
        // land in separate groups so each renders its own snap.
        let v1 = bin_variant("dist/anodizer-v1", "x86_64-unknown-linux-gnu", None);
        let v3 = bin_variant("dist/anodizer-v3", "x86_64-unknown-linux-gnu", Some("v3"));
        let all = vec![&v1, &v3];

        let groups = group_binaries_by_target(&all);
        assert_eq!(groups.len(), 2, "two variants form two distinct groups");
        assert!(groups.contains_key(&("x86_64-unknown-linux-gnu".to_string(), None)));
        assert!(groups.contains_key(&(
            "x86_64-unknown-linux-gnu".to_string(),
            Some("v3".to_string())
        )));
    }

    /// The default snap name must equal core's default asset-name stem plus
    /// `.snap` for every target shape (arm split, mips whole-token, amd64
    /// baseline) — snaps carry the same name every sibling artifact derives
    /// for the target, never a privately-translated scheme.
    #[test]
    fn default_snap_name_equals_core_default_stem() {
        use anodizer_core::test_helpers::TestContextBuilder;
        let cases: &[(&str, &str)] = &[
            ("x86_64-unknown-linux-gnu", "mysnap_1.2.3_linux_amd64.snap"),
            ("aarch64-unknown-linux-gnu", "mysnap_1.2.3_linux_arm64.snap"),
            (
                "armv7-unknown-linux-gnueabihf",
                "mysnap_1.2.3_linux_armv7.snap",
            ),
            (
                "mips64el-unknown-linux-gnuabi64",
                "mysnap_1.2.3_linux_mips64el.snap",
            ),
        ];
        for (target, expected) in cases {
            let mut ctx = TestContextBuilder::new()
                .project_name("myapp")
                .tag("v1.2.3")
                .build();
            let (os, arch) = anodizer_core::target::map_target(target);
            let name = compute_snap_filename(
                &mut ctx,
                &SnapcraftConfig::default(),
                "myapp",
                "mysnap",
                Some(target),
                &os,
                &arch,
                None,
            )
            .expect("render snap filename");
            assert_eq!(&name, expected, "snap name for {target}");

            // Cross-check against core's own render of the same default.
            let mut core_ctx = TestContextBuilder::new()
                .project_name("myapp")
                .tag("v1.2.3")
                .build();
            core_ctx.template_vars_mut().set("ProjectName", "mysnap");
            let stem = anodizer_core::archive_name::render_archive_stem(
                &mut core_ctx,
                anodizer_core::archive_name::DEFAULT_NAME_TEMPLATE,
                target,
            )
            .unwrap();
            assert_eq!(
                name,
                format!("{stem}.snap"),
                "core stem parity for {target}"
            );
        }
    }

    #[test]
    fn same_triple_multi_variant_yields_distinct_snap_names() {
        // 3 amd64 variants of one triple + 1 arm64 → 4 distinct .snap filenames
        // under the default template (no clobber).
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .tag("v1.2.3")
            .build();

        let snap_cfg = SnapcraftConfig::default();
        let cases: [(&str, &str, Option<&str>); 4] = [
            ("amd64", "x86_64-unknown-linux-gnu", None),
            ("amd64", "x86_64-unknown-linux-gnu", Some("v2")),
            ("amd64", "x86_64-unknown-linux-gnu", Some("v3")),
            ("arm64", "aarch64-unknown-linux-gnu", None),
        ];

        let mut names = std::collections::HashSet::new();
        for (arch, target, variant) in cases {
            let name = compute_snap_filename(
                &mut ctx,
                &snap_cfg,
                "anodizer",
                "anodizer",
                Some(target),
                "linux",
                arch,
                variant,
            )
            .expect("render snap filename");
            assert!(names.insert(name.clone()), "duplicate snap name: {name}");
        }
        assert_eq!(
            names.len(),
            4,
            "all four targets/variants produce distinct names"
        );
    }

    #[test]
    fn host_build_resets_every_variant_var() {
        // A host-target (no triple) render after an aarch64 target render:
        // the stale `Arm64="v8"` (and any other variant var) must not leak
        // into a user name_template that references it.
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.2.3")
            .build();

        // Prior target render seeds Arm64="v8" (plus I386 via a 386 pass).
        ctx.template_vars_mut().set("Arm64", "v8");
        ctx.template_vars_mut().set("I386", "sse2");
        ctx.template_vars_mut().set("Arm", "7");
        ctx.template_vars_mut().set("Mips", "stale");

        let snap_cfg = SnapcraftConfig {
            name_template: Some(
                "{{ ProjectName }}_{{ Os }}_{{ Arch }}{{ Arm }}{{ Arm64 }}{{ Amd64 }}{{ Mips }}{{ I386 }}"
                    .to_string(),
            ),
            ..Default::default()
        };
        let name = compute_snap_filename(
            &mut ctx, &snap_cfg, "myapp", "mysnap", None, "linux", "amd64", None,
        )
        .expect("render snap filename");
        // Amd64 renders the unified untagged baseline; every other variant
        // var must be empty — no `v8`/`sse2`/`7`/`stale` leak.
        assert_eq!(name, "mysnap_linux_amd64v1.snap");
    }
}
