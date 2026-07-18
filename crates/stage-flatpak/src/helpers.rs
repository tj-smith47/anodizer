use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;

use super::*;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
pub(crate) fn os_arch_from_target(target: Option<&str>) -> (String, String) {
    anodizer_core::target::os_arch_with_default(target, "linux")
}

/// Resolve `extra_files` specs to `(source_path, destination_filename)` pairs.
///
/// Walks the configured glob patterns, keeps only regular files, and pairs
/// each match with the spec's `name_template` (or, if unset, the source
/// basename). Invalid glob patterns are warned-and-skipped to match the
/// previous in-place behaviour. Centralises the spec-iteration shape so the
/// manifest-emission path and the copy-into-workdir path agree on which
/// files participate and what filename they take in `/app/share/<id>/`.
pub(crate) fn resolve_extra_file_specs(
    specs: &[anodizer_core::config::ExtraFileSpec],
    log: &anodizer_core::log::StageLogger,
) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for spec in specs {
        let pattern = spec.glob();
        match glob::glob(pattern) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if entry.is_file() {
                        let dst_name = spec
                            .name_template()
                            .map(|s| s.to_string())
                            .or_else(|| {
                                entry
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| "extra".to_string());
                        out.push((entry, dst_name));
                    }
                }
            }
            Err(e) => {
                log.warn(&format!(
                    "invalid extra_files glob pattern '{}': {}",
                    pattern, e
                ));
            }
        }
    }
    out
}

/// A fully-staged flatpak job. Step 1 (serial, `&mut ctx`) stages the
/// work directory, writes the manifest, and applies mod_timestamp to the
/// work dir; Step 2 (parallel, `std::thread::scope`) runs the two
/// `flatpak-builder` / `flatpak build-bundle` subprocesses and applies
/// mod_timestamp to the output file.
pub(crate) struct FlatpakJob {
    pub(crate) work_dir: PathBuf,
    pub(crate) output_name: String,
    /// Process-cwd-relative bundle path, by design: the parallel phase's
    /// `set_file_mtime` runs inside the Rust process whose cwd is the dist
    /// root, so a dist-relative path resolves correctly here. `bundle_args`
    /// carries the *absolutized* variant instead, because the `build-bundle`
    /// subprocess runs with cwd set to `work_dir` (where a relative path
    /// would resolve under the work dir and fail).
    pub(crate) output_path: PathBuf,
    pub(crate) builder_args: Vec<String>,
    pub(crate) bundle_args: Vec<String>,
    /// Pre-parsed mtime to stamp the output `.flatpak` with; when set,
    /// the parallel phase also calls `set_file_mtime`. The serial phase
    /// already stamped the work dir.
    pub(crate) output_mtime: Option<std::time::SystemTime>,
    /// Rendered mod_timestamp string for logging.
    pub(crate) output_mtime_repr: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) crate_name: String,
    pub(crate) cfg_id: Option<String>,
    /// The binary's amd64 micro-architecture variant (`None` / `Some("v1")`
    /// → baseline), recorded in the produced artifact's metadata so downstream
    /// stages can tell two amd64 builds of one target apart.
    pub(crate) amd64_variant: Option<String>,
}

/// Collect crates that declare at least one `flatpaks:` config and are not
/// excluded by the active `--crates` selection.
pub(crate) fn collect_flatpak_crates(
    ctx: &Context,
    selected: &[String],
) -> Vec<anodizer_core::config::CrateConfig> {
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.flatpaks.is_some())
        .cloned()
        .collect()
}

/// Returns true when at least one flatpak config across the supplied crates is
/// not skipped by its `skip:` template.
pub(crate) fn any_flatpak_enabled(
    ctx: &Context,
    crates: &[anodizer_core::config::CrateConfig],
) -> Result<bool> {
    for c in crates {
        let Some(cfgs) = c.flatpaks.as_ref() else {
            continue;
        };
        for cfg in cfgs {
            let off = match cfg.skip.as_ref() {
                Some(d) => d
                    .try_evaluates_to_true(|s| ctx.render_template(s))
                    .with_context(|| {
                        format!("flatpak: render skip template for crate {}", c.name)
                    })?,
                None => false,
            };
            if !off {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Confirm both `flatpak-builder` and `flatpak` are on PATH; bail otherwise.
pub(crate) fn require_flatpak_tools() -> Result<()> {
    if !anodizer_core::tool_detect::on_path("flatpak-builder") {
        anyhow::bail!(
            "flatpak-builder not found on PATH; install Flatpak to create Flatpak bundles"
        );
    }
    if !anodizer_core::tool_detect::on_path("flatpak") {
        anyhow::bail!("flatpak not found on PATH; install Flatpak to create Flatpak bundles");
    }
    Ok(())
}

/// Resolve the `Version` template variable, warning and defaulting when unset.
pub(crate) fn resolve_flatpak_version(
    ctx: &Context,
    log: &anodizer_core::log::StageLogger,
) -> String {
    ctx.template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| {
            log.warn("no Version template variable set; using 0.0.0 for Flatpak bundle version");
            "0.0.0".to_string()
        })
}

/// Validate the four required fields on a `FlatpakConfig` and return their
/// non-empty `&str` views.
pub(crate) fn validate_flatpak_required_fields<'a>(
    flatpak_cfg: &'a anodizer_core::config::FlatpakConfig,
    crate_name: &str,
) -> Result<(&'a str, &'a str, &'a str, &'a str)> {
    let app_id = flatpak_cfg
        .app_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("flatpak: app_id is required for crate '{}'", crate_name))?;
    let runtime = flatpak_cfg
        .runtime
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("flatpak: runtime is required for crate '{}'", crate_name)
        })?;
    let runtime_version = flatpak_cfg
        .runtime_version
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "flatpak: runtime_version is required for crate '{}'",
                crate_name
            )
        })?;
    let sdk = flatpak_cfg
        .sdk
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("flatpak: sdk is required for crate '{}'", crate_name))?;
    Ok((app_id, runtime, runtime_version, sdk))
}

/// Collect Linux binary artifacts for the given crate.
pub(crate) fn collect_linux_binaries(ctx: &Context, crate_name: &str) -> Vec<Artifact> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_linux)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Filter linux binaries to those matching the `ids:` allow-list (build id or
/// binary name). A `None` or empty filter is a no-op.
pub(crate) fn filter_binaries_by_ids(
    binaries: &mut Vec<Artifact>,
    filter_ids: Option<&Vec<String>>,
) {
    if let Some(filter_ids) = filter_ids
        && !filter_ids.is_empty()
    {
        binaries.retain(|b| {
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
}

/// One Flatpak-buildable binary: `(target, amd64_variant, binary_path,
/// flatpak_arch)`. `amd64_variant` is the binary's `amd64_variant` metadata
/// (`None` / `Some("v1")` → no name suffix), carried so two amd64 builds of one
/// triple stay distinct.
pub(crate) type FlatpakBinary = (Option<String>, Option<String>, PathBuf, String);

/// Map filtered binaries onto [`FlatpakBinary`] tuples, dropping any
/// architecture Flatpak doesn't support.
///
/// Deduplicates by `(flatpak_arch, amd64_variant)` so at most one binary
/// survives per Flatpak arch *and* micro-architecture variant. Two builds that
/// share an arch *and* a variant — e.g. an x86_64 gnu build and an x86_64 musl
/// build, both baseline amd64 — collapse onto one job: the per-job work dir and
/// bundle filename are keyed by `(crate, flatpak_arch, variant)`, so they would
/// target the identical `dist/flatpak/<crate>/<arch>/build` dir and identical
/// `..._linux_amd64.flatpak` output. Run in parallel that races (`Build
/// directory already initialized`); run serially the second clobbers the first.
/// Two amd64 builds tagged with *different* variants (`v1` + `v3`) keep distinct
/// keys, so both survive and render distinct names.
///
/// The dedup is a last-resort guard: which binary wins is order-dependent
/// (first in the artifact list), so it must not be relied on to select the
/// "right" build. The intended selector when gnu and musl collapse to one
/// Flatpak arch is the config's `ids:` filter (applied upstream in
/// [`process_flatpak_cfg`]) — binding the `flatpaks:` block to a single build,
/// exactly as `snapcrafts:` does to avoid the same map_target collapse.
pub(crate) fn map_to_supported_arches(binaries: &[Artifact]) -> Vec<FlatpakBinary> {
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let mut out: Vec<FlatpakBinary> = Vec::new();
    for b in binaries {
        let (_, arch) = os_arch_from_target(b.target.as_deref());
        if let Some(flatpak_arch) = arch_to_flatpak(&arch) {
            let variant = b.metadata.get("amd64_variant").cloned();
            let key = (flatpak_arch.to_string(), variant.clone());
            if seen.iter().any(|k| k == &key) {
                continue;
            }
            seen.push(key);
            out.push((
                b.target.clone(),
                variant,
                b.path.clone(),
                flatpak_arch.to_string(),
            ));
        }
    }
    out
}

/// The rendered bundle filename paired with the template that produced it:
/// `(rendered_name, resolved_template)`. The resolved template is the user's
/// `name_template` when set, else the composed default — exactly the string the
/// [`ArchPathGuard`] cites when it rejects a clobber, so the diagnostic names
/// the template the user can fix.
pub(crate) type RenderedFilename = (String, String);

/// Render the bundle output filename via `name_template`, defaulting to
/// `default_name`, and force a `.flatpak` suffix. Returns the rendered name
/// alongside the resolved template so the single resolution feeds both the
/// produced path and the clobber guard.
pub(crate) fn render_output_filename(
    ctx: &Context,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    crate_name: &str,
    target: &Option<String>,
    default_name: &str,
) -> Result<RenderedFilename> {
    let name_template = flatpak_cfg.name_template.as_deref().unwrap_or(default_name);
    let rendered = ctx.render_template(name_template).with_context(|| {
        format!(
            "flatpak: render name template for crate {} target {:?}",
            crate_name, target
        )
    })?;
    let output_name = if rendered.to_lowercase().ends_with(".flatpak") {
        rendered
    } else {
        format!("{rendered}.flatpak")
    };
    Ok((output_name, name_template.to_string()))
}

/// Build the manifest JSON model plus the parallel `(sources, build_commands)`
/// vectors. Returns the assembled [`Manifest`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_manifest(
    app_id: &str,
    runtime: &str,
    runtime_version: &str,
    sdk: &str,
    command: &str,
    finish_args: Vec<String>,
    binary_name: &str,
    extra_file_names: &[String],
) -> Manifest {
    let mut sources = vec![ManifestSource {
        type_: "file".to_string(),
        path: binary_name.to_string(),
        dest_filename: None,
    }];
    let mut build_commands = vec![format!(
        "install -Dm755 {binary_name} /app/bin/{binary_name}"
    )];

    for extra_name in extra_file_names {
        sources.push(ManifestSource {
            type_: "file".to_string(),
            path: extra_name.clone(),
            dest_filename: None,
        });
        build_commands.push(format!(
            "install -Dm644 {extra_name} /app/share/{app_id}/{extra_name}"
        ));
    }

    Manifest {
        id: app_id.to_string(),
        runtime: runtime.to_string(),
        runtime_version: runtime_version.to_string(),
        sdk: sdk.to_string(),
        command: command.to_string(),
        finish_args,
        modules: vec![ManifestModule {
            name: app_id.to_string(),
            buildsystem: "simple".to_string(),
            build_commands,
            sources,
        }],
    }
}

/// Build the dry-run [`Artifact`] for a single Flatpak bundle, logging the
/// would-do messages.
pub(crate) fn dry_run_artifact(
    log: &anodizer_core::log::StageLogger,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    crate_name: &str,
    target: &Option<String>,
    amd64_variant: Option<&str>,
    output_name: &str,
    output_path: PathBuf,
) -> Artifact {
    log.status(&format!(
        "(dry-run) would create Flatpak bundle {} for crate {} target {:?}",
        output_name, crate_name, target
    ));

    if let Some(ts) = &flatpak_cfg.mod_timestamp {
        log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
    }

    let mut metadata = HashMap::from([("format".to_string(), "flatpak".to_string())]);
    if let Some(id) = &flatpak_cfg.id {
        metadata.insert("id".to_string(), id.clone());
    }
    if let Some(v) = amd64_variant {
        metadata.insert("amd64_variant".to_string(), v.to_string());
    }
    Artifact {
        kind: ArtifactKind::Flatpak,
        name: String::new(),
        path: output_path,
        target: target.clone(),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    }
}

/// Compose the two subprocess argument vectors used by the parallel phase.
pub(crate) fn build_subprocess_args(
    app_id: &str,
    version: &str,
    flatpak_arch: &str,
    output_path: &std::path::Path,
) -> (Vec<String>, Vec<String>) {
    let builder_args = vec![
        "flatpak-builder".to_string(),
        "--force-clean".to_string(),
        // Disable the rofiles-fuse read-only overlay flatpak-builder mounts
        // under `.flatpak-builder/rofiles/` during the build. It is purely a
        // build-time guard against modifying cached sources (the produced
        // ostree/bundle bytes are identical with or without it), but the FUSE
        // mount is unreliable in containers/CI and can leak past the build —
        // a stale `fuse.rofiles-fuse` mount inside the determinism harness's
        // per-run git worktree makes `git worktree remove` fail with "Device
        // or resource busy" and aborts the second run. Disabling it keeps the
        // build hermetic without leaving a mount behind.
        "--disable-rofiles-fuse".to_string(),
        format!("--arch={flatpak_arch}"),
        format!("--default-branch={version}"),
        "--repo=repo".to_string(),
        "build".to_string(),
        format!("{app_id}.json"),
    ];
    let bundle_args = vec![
        "flatpak".to_string(),
        "build-bundle".to_string(),
        format!("--arch={flatpak_arch}"),
        "repo".to_string(),
        output_path.to_string_lossy().into_owned(),
        app_id.to_string(),
        version.to_string(),
    ];
    (builder_args, bundle_args)
}

/// Stage the work directory for a Flatpak job: copy binary + extra files,
/// write the manifest JSON, and apply mod_timestamp to the work dir.
/// Returns the pre-parsed `(mtime, repr)` to stamp the output file later.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_work_dir(
    ctx: &Context,
    log: &anodizer_core::log::StageLogger,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    work_dir: &std::path::Path,
    output_dir: &std::path::Path,
    binary_path: &std::path::Path,
    binary_name: &str,
    manifest: &Manifest,
    app_id: &str,
) -> Result<(Option<std::time::SystemTime>, Option<String>)> {
    fs::create_dir_all(work_dir)
        .with_context(|| format!("create Flatpak work dir: {}", work_dir.display()))?;
    fs::create_dir_all(output_dir)
        .with_context(|| format!("create Flatpak output dir: {}", output_dir.display()))?;

    let staged_binary = work_dir.join(binary_name);
    fs::copy(binary_path, &staged_binary).with_context(|| {
        format!(
            "copy binary {} to {}",
            binary_path.display(),
            staged_binary.display()
        )
    })?;

    // The staged binary must be executable so `install -Dm755` inside the
    // sandbox produces a runnable `/app/bin/<name>`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&staged_binary, perms).with_context(|| {
            format!(
                "flatpak: set executable permission on {}",
                staged_binary.display()
            )
        })?;
    }

    if let Some(extra_files) = &flatpak_cfg.extra_files {
        for (entry, dst_name) in resolve_extra_file_specs(extra_files, log) {
            let dst = work_dir.join(&dst_name);
            fs::copy(&entry, &dst)
                .with_context(|| format!("copy extra file {} to work dir", entry.display()))?;
        }
    }

    let manifest_json =
        serde_json::to_string_pretty(manifest).context("flatpak: serialize manifest JSON")?;
    let manifest_path = work_dir.join(format!("{app_id}.json"));
    fs::write(&manifest_path, &manifest_json)
        .with_context(|| format!("flatpak: write manifest to {}", manifest_path.display()))?;

    if let Some(ref ts_tmpl) = flatpak_cfg.mod_timestamp {
        let ts = ctx
            .render_template(ts_tmpl)
            .with_context(|| "flatpak: render mod_timestamp template")?;
        anodizer_core::util::apply_mod_timestamp(work_dir, &ts, log)?;
        let mtime = anodizer_core::util::parse_mod_timestamp(&ts)?;
        Ok((Some(mtime), Some(ts)))
    } else {
        Ok((None, None))
    }
}

/// Run a single [`FlatpakJob`] in the parallel phase: invoke flatpak-builder,
/// then `flatpak build-bundle`, then stamp the output file's mtime if set.
pub(crate) fn run_flatpak_job(
    job: &FlatpakJob,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Artifact> {
    let thread_log = anodizer_core::log::StageLogger::new("flatpak", verbosity);

    thread_log.verbose(&format!("running {}", job.builder_args.join(" ")));
    let output = Command::new(&job.builder_args[0])
        .args(&job.builder_args[1..])
        .current_dir(&job.work_dir)
        .output()
        .with_context(|| {
            format!(
                "execute flatpak-builder for crate {} target {:?}",
                job.crate_name, job.target
            )
        })?;
    thread_log.check_output(output, "flatpak-builder")?;

    thread_log.verbose(&format!("running {}", job.bundle_args.join(" ")));
    let output = Command::new(&job.bundle_args[0])
        .args(&job.bundle_args[1..])
        .current_dir(&job.work_dir)
        .output()
        .with_context(|| {
            format!(
                "execute flatpak build-bundle for crate {} target {:?}",
                job.crate_name, job.target
            )
        })?;
    thread_log.check_output(output, "flatpak build-bundle")?;

    // Remove the flatpak-builder scratch tree (ostree `repo/`, the
    // `.flatpak-builder/` object cache, the `build/` checkout, the staged
    // manifest + binary). The self-contained `.flatpak` bundle is already
    // written to `output_path`, which sits OUTSIDE this dir, so the scratch is
    // pure build intermediate with no downstream consumer. Leaving it under
    // `dist/` both clutters the output and — because ostree commit objects,
    // refs, and summaries embed per-build timestamps and member ordering —
    // makes the determinism harness's artifact walker diff non-reproducible
    // intermediates that never ship. Best-effort: a failed cleanup must not
    // fail an otherwise-successful bundle.
    if let Err(e) = std::fs::remove_dir_all(&job.work_dir) {
        thread_log.verbose(&format!(
            "flatpak: could not remove build scratch {}: {e}",
            job.work_dir.display()
        ));
    }

    if let (Some(mtime), Some(repr)) = (job.output_mtime, job.output_mtime_repr.as_deref())
        && job.output_path.exists()
    {
        anodizer_core::util::set_file_mtime(&job.output_path, mtime)?;
        thread_log.status(&format!(
            "applied mod_timestamp={repr} to {}",
            job.output_path.display()
        ));
    }

    thread_log.status(&format!(
        "created Flatpak bundle {} for crate {} target {:?}",
        job.output_name, job.crate_name, job.target
    ));

    let mut metadata = HashMap::from([("format".to_string(), "flatpak".to_string())]);
    if let Some(id) = &job.cfg_id {
        metadata.insert("id".to_string(), id.clone());
    }
    if let Some(v) = &job.amd64_variant {
        metadata.insert("amd64_variant".to_string(), v.clone());
    }
    Ok(Artifact {
        kind: ArtifactKind::Flatpak,
        name: String::new(),
        path: job.output_path.clone(),
        target: job.target.clone(),
        crate_name: job.crate_name.clone(),
        metadata,
        size: None,
    })
}
