use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::arch::{is_valid_snap_arch, triple_to_snap_arch};
use crate::command::snapcraft_command;
use crate::generate::generate_snap_yaml;
use crate::yaml::DEFAULT_SNAP_NAME_TEMPLATE;

// Workaround for snapcraft 8.14.5: `snapcraft_legacy.internal.repo._deb` runs
// `BaseDirectory.save_cache_path("snapcraft", "download")` at import time, which
// calls `os.makedirs(path)` without `exist_ok=True`. Once the first invocation
// creates that directory, every subsequent snapcraft process crashes at import
// before it can pack. We wipe the cache dir and serialize invocations so the
// wipe-then-pack sequence is atomic across parallel workers.
static SNAPCRAFT_CACHE_LOCK: Mutex<()> = Mutex::new(());

fn clear_snapcraft_cache() {
    if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(home).join(".cache/snapcraft/download");
        let _ = std::fs::remove_dir_all(&cache);
    }
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
                // Skip configs marked skip:
                if let Some(ref d) = snap_cfg.skip {
                    let off = d
                        .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .with_context(|| {
                            format!("snapcraft: render skip template for crate {}", krate.name)
                        })?;
                    if off {
                        log.status(&format!(
                            "skipping snapcraft config for crate {} (skip=true)",
                            krate.name
                        ));
                        continue;
                    }
                }

                // Validate confinement value
                if let Some(conf) = &snap_cfg.confinement {
                    match conf.as_str() {
                        "strict" | "devmode" | "classic" => {}
                        other => anyhow::bail!(
                            "snapcraft: invalid confinement '{}' for crate '{}'. \
                             Valid values are: strict, devmode, classic",
                            other,
                            krate.name
                        ),
                    }
                }

                // Validate grade value
                if let Some(grade) = &snap_cfg.grade {
                    match grade.as_str() {
                        "stable" | "devel" => {}
                        other => anyhow::bail!(
                            "snapcraft: invalid grade '{}' for crate '{}'. \
                             Valid values are: stable, devel",
                            other,
                            krate.name
                        ),
                    }
                }

                // Warn early about the `icon:` Snap Store leak: the field
                // round-trips through snap.yaml into snap.json, where the
                // Store's schema rejects it ("Additional properties are
                // not allowed ('icon' was unexpected)"). The supported
                // path is `snap/gui/<name>.png` in the project tree.
                // Surface this BEFORE snapcraft pack burns minutes only to
                // have `snapcraft upload` blow up at validation.
                if snap_cfg.icon.is_some() {
                    log.warn(&format!(
                        "snapcraft: 'icon' is set for crate '{}' — the Snap \
                         Store rejects snap.json with this field. Move the \
                         icon to snap/gui/<name>.png in your project tree \
                         (which is not copied into snap.json) and remove \
                         the 'icon:' key from the snapcrafts block",
                        krate.name
                    ));
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
                    let target = if target_key == "unknown" {
                        None
                    } else {
                        Some(target_key.clone())
                    };

                    // skip unsupported
                    // architectures (e.g. riscv64 is not in the snap store).
                    if let Some(ref t) = target {
                        let snap_arch = triple_to_snap_arch(t);
                        if !is_valid_snap_arch(snap_arch) {
                            log.warn(&format!(
                                "snapcraft: skipping unsupported arch '{}' (target: {})",
                                snap_arch, t
                            ));
                            continue;
                        }
                    }

                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = target
                        .as_deref()
                        .map(anodizer_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    // Ensure output directory exists
                    let output_dir = dist.join("linux");
                    if !dry_run {
                        fs::create_dir_all(&output_dir).with_context(|| {
                            format!("create snapcraft output dir: {}", output_dir.display())
                        })?;
                    }

                    // Determine output filename from name_template or default.
                    // Matches GoReleaser's defaultNameTemplate (snapcraft.go:103):
                    //   {{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}{{ with .Mips }}_{{ . }}{{ end }}{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}
                    let snap_name = snap_cfg.name.as_deref().unwrap_or(&krate.name);
                    // Save ProjectName to restore after render — we override it with
                    // snap_name so per-crate default filenames don't collide.
                    let saved_project_name = ctx
                        .template_vars()
                        .get("ProjectName")
                        .cloned()
                        .unwrap_or_default();
                    ctx.template_vars_mut().set("ProjectName", snap_name);
                    ctx.template_vars_mut().set("Os", &os);
                    // For ARM targets, split Arch="arm" and Arm="6"/"7" so the
                    // default template (concatenating `{{ .Arch }}v{{ .Arm }}`)
                    // produces "armv6" rather than "armv6v6".
                    if let Some(version) = arch.strip_prefix("armv") {
                        ctx.template_vars_mut().set("Arch", "arm");
                        ctx.template_vars_mut().set("Arm", version);
                    } else {
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut().set("Arm", "");
                    }
                    ctx.template_vars_mut()
                        .set("Amd64", if arch == "amd64" { "v1" } else { "" });
                    ctx.template_vars_mut().set("Mips", "");
                    ctx.template_vars_mut()
                        .set("Target", target.as_deref().unwrap_or(""));
                    let tmpl = snap_cfg
                        .name_template
                        .as_deref()
                        .unwrap_or(DEFAULT_SNAP_NAME_TEMPLATE);
                    let render_result = ctx.render_template(tmpl).with_context(|| {
                        format!(
                            "snapcraft: render name_template for crate {} target {:?}",
                            krate.name, target
                        )
                    });
                    ctx.template_vars_mut()
                        .set("ProjectName", &saved_project_name);
                    let rendered = render_result?;
                    let snap_filename = if rendered.to_lowercase().ends_with(".snap") {
                        rendered
                    } else {
                        format!("{rendered}.snap")
                    };
                    let snap_path = output_dir.join(&snap_filename);

                    // Build artifact metadata (I4)
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
                            krate.name,
                            target,
                        ));
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Snap,
                            name: String::new(),
                            path: snap_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: artifact_metadata,
                            size: None,
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        archives_to_remove.extend(anodizer_core::util::collect_if_replace(
                            snap_cfg.replace,
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));

                        continue;
                    }

                    // pre-stage binaries
                    // and extra files into a prime directory, write snap.yaml to
                    // prime/meta/snap.yaml, then run `snapcraft pack prime_dir`.
                    let tmp_dir =
                        tempfile::tempdir().context("create temp dir for snapcraft build")?;
                    let prime_dir = tmp_dir.path().join("prime");
                    let meta_dir = prime_dir.join("meta");
                    fs::create_dir_all(&meta_dir).with_context(|| {
                        format!("create prime/meta dir: {}", meta_dir.display())
                    })?;

                    // Collect all binary names for this platform group
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
                    let binary_name_refs: Vec<&str> =
                        all_binary_names.iter().map(|s| s.as_str()).collect();

                    // GoReleaser renders summary, description, and grade
                    // through its template engine before generating the YAML.
                    // GoReleaser Pro parity: fall back to project `metadata.description`
                    // when snapcraft config's `description` is unset.
                    let mut rendered_cfg = snap_cfg.clone();
                    if rendered_cfg.description.is_none() {
                        rendered_cfg.description =
                            ctx.config.meta_description().map(str::to_string);
                    }
                    if let Some(ref s) = rendered_cfg.summary {
                        rendered_cfg.summary = Some(ctx.render_template(s).with_context(|| {
                            format!("snapcraft: render summary for crate {}", krate.name)
                        })?);
                    }
                    if let Some(ref d) = rendered_cfg.description {
                        rendered_cfg.description =
                            Some(ctx.render_template(d).with_context(|| {
                                format!("snapcraft: render description for crate {}", krate.name)
                            })?);
                    }
                    if let Some(ref g) = rendered_cfg.grade {
                        rendered_cfg.grade = Some(ctx.render_template(g).with_context(|| {
                            format!("snapcraft: render grade for crate {}", krate.name)
                        })?);
                    }

                    // Generate and write snap.yaml to prime/meta/snap.yaml
                    let project_name = &ctx.config.project_name;
                    let yaml_content = generate_snap_yaml(
                        &rendered_cfg,
                        &version,
                        &binary_name_refs,
                        target.as_deref(),
                        Some(project_name.as_str()),
                    )?;
                    let yaml_path = meta_dir.join("snap.yaml");
                    fs::write(&yaml_path, &yaml_content)
                        .with_context(|| format!("write snap.yaml to {}", yaml_path.display()))?;

                    // copy binaries
                    // directly into the prime directory root with mode 0555.
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
                            std::fs::set_permissions(&binary_dest, perms).with_context(|| {
                                format!("set binary mode 0555 on {}", binary_dest.display())
                            })?;
                        }
                    }

                    // copy extra files
                    // into the prime directory at their destination paths.
                    if let Some(extra_files) = &snap_cfg.extra_files {
                        for extra in extra_files {
                            let src = PathBuf::from(extra.source());
                            let dest_rel = extra.destination().unwrap_or_else(|| extra.source());
                            let dest = prime_dir.join(dest_rel);
                            if let Some(parent) = dest.parent() {
                                fs::create_dir_all(parent).with_context(|| {
                                    format!("create dir for extra file: {}", parent.display())
                                })?;
                            }
                            fs::copy(&src, &dest).with_context(|| {
                                format!("copy extra file {} to {}", src.display(), dest.display())
                            })?;
                            // Apply file mode: GoReleaser defaults extra
                            // file mode to 0644 when not specified.
                            #[cfg(unix)]
                            {
                                let mode = extra.mode().unwrap_or(0o644);
                                if mode > 0o7777 {
                                    anyhow::bail!(
                                        "snapcraft: invalid file mode {:o} for '{}' — \
                                         must be in range 0-7777 (octal)",
                                        mode,
                                        src.display()
                                    );
                                }
                                use std::os::unix::fs::PermissionsExt;
                                let perms = std::fs::Permissions::from_mode(mode);
                                std::fs::set_permissions(&dest, perms).with_context(|| {
                                    format!("set mode {:o} on {}", mode, dest.display())
                                })?;
                            }
                        }
                    }

                    // copy completer files
                    // referenced by app configs into the prime directory.
                    if let Some(ref apps_map) = snap_cfg.apps {
                        for app_cfg in apps_map.values() {
                            if let Some(ref completer_path) = app_cfg.completer {
                                let src = PathBuf::from(completer_path);
                                let dest = prime_dir.join(completer_path);
                                if let Some(parent) = dest.parent() {
                                    fs::create_dir_all(parent).with_context(|| {
                                        format!(
                                            "snapcraft: create dir for completer {}",
                                            parent.display()
                                        )
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
                        }
                    }

                    // Process templated_extra_files: render and copy to prime dir
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

                    // Apply mod_timestamp if set
                    if let Some(ts) = &snap_cfg.mod_timestamp {
                        anodizer_core::util::apply_mod_timestamp(&prime_dir, ts, &log)?;
                    }

                    // Step 1 done: compose subprocess args and hand the
                    // staged work to the parallel worker pool.
                    let cmd_args = snapcraft_command(
                        &prime_dir.to_string_lossy(),
                        &snap_path.to_string_lossy(),
                    );

                    // If replace is set, mark archives for this crate+target
                    // for removal — do it now while ctx.artifacts is accessible.
                    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
                        snap_cfg.replace,
                        &ctx.artifacts,
                        &krate.name,
                        target.as_deref(),
                    ));

                    jobs.push(SnapcraftJob {
                        _tmp_dir: tmp_dir,
                        snap_path,
                        cmd_args,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        artifact_metadata,
                    });
                }
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        // ----------------------------------------------------------------
        // Step 2 (parallel): run `snapcraft pack` per job. Bounded
        // concurrency via chunks(parallelism). Each worker returns the
        // populated Artifact; Step 3 registers them serially.
        // ----------------------------------------------------------------
        if !jobs.is_empty() {
            let run_job = |job: &SnapcraftJob| -> Result<Artifact> {
                let thread_log = anodizer_core::log::StageLogger::new("snapcraft", log.verbosity());

                // Serialize wipe-then-pack across parallel workers so each
                // snapcraft invocation sees a non-existent cache dir at import
                // time. See SNAPCRAFT_CACHE_LOCK comment at the top of the file.
                let _cache_guard = SNAPCRAFT_CACHE_LOCK
                    .lock()
                    .map_err(|_| anyhow::anyhow!("snapcraft cache lock poisoned"))?;
                clear_snapcraft_cache();

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

            let results = anodizer_core::parallel::run_parallel_chunks(
                &jobs,
                parallelism,
                "snapcraft",
                run_job,
            )?;
            new_artifacts.extend(results);
        }

        // Remove replaced archives
        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}
