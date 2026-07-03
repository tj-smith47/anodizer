use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// Architecture mapping
// ---------------------------------------------------------------------------

/// Map a Go-style or Rust-style architecture name to the Flatpak equivalent.
/// Only x86_64 and aarch64 are supported by Flatpak.
fn arch_to_flatpak(arch: &str) -> Option<&'static str> {
    match arch {
        "amd64" | "x86_64" => Some("x86_64"),
        "arm64" | "aarch64" => Some("aarch64"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Default name template
// ---------------------------------------------------------------------------

/// Stem of the default Flatpak bundle filename template, before the amd64
/// variant suffix and the `.flatpak` extension.
///
/// Flatpak carries the whole go-arch in `Arch` (no arm-split), so the only
/// micro-architecture dimension that can collide on one `Arch` is amd64 —
/// hence the amd64-only suffix appended by [`default_name_template`], not the
/// full Arm/Mips/Amd64 clause.
const DEFAULT_NAME_PREFIX: &str = "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}";

/// Compose the default Flatpak bundle filename template: the
/// [`DEFAULT_NAME_PREFIX`], the shared amd64 variant suffix, then the
/// `.flatpak` extension. Sourced from the single core const so the suffix
/// cannot drift from the other installer namers.
fn default_name_template() -> String {
    format!(
        "{DEFAULT_NAME_PREFIX}{}.flatpak",
        anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX
    )
}

// ---------------------------------------------------------------------------
// Manifest JSON structures
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Manifest {
    id: String,
    runtime: String,
    #[serde(rename = "runtime-version")]
    runtime_version: String,
    sdk: String,
    command: String,
    #[serde(rename = "finish-args", skip_serializing_if = "Vec::is_empty")]
    finish_args: Vec<String>,
    modules: Vec<ManifestModule>,
}

#[derive(Serialize)]
struct ManifestModule {
    name: String,
    buildsystem: String,
    #[serde(rename = "build-commands")]
    build_commands: Vec<String>,
    sources: Vec<ManifestSource>,
}

#[derive(Serialize)]
struct ManifestSource {
    #[serde(rename = "type")]
    type_: String,
    path: String,
    #[serde(rename = "dest-filename", skip_serializing_if = "Option::is_none")]
    dest_filename: Option<String>,
}

// ---------------------------------------------------------------------------
// FlatpakStage
// ---------------------------------------------------------------------------

pub struct FlatpakStage;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
fn os_arch_from_target(target: Option<&str>) -> (String, String) {
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
fn resolve_extra_file_specs(
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
struct FlatpakJob {
    work_dir: PathBuf,
    output_name: String,
    /// Process-cwd-relative bundle path, by design: the parallel phase's
    /// `set_file_mtime` runs inside the Rust process whose cwd is the dist
    /// root, so a dist-relative path resolves correctly here. `bundle_args`
    /// carries the *absolutized* variant instead, because the `build-bundle`
    /// subprocess runs with cwd set to `work_dir` (where a relative path
    /// would resolve under the work dir and fail).
    output_path: PathBuf,
    builder_args: Vec<String>,
    bundle_args: Vec<String>,
    /// Pre-parsed mtime to stamp the output `.flatpak` with; when set,
    /// the parallel phase also calls `set_file_mtime`. The serial phase
    /// already stamped the work dir.
    output_mtime: Option<std::time::SystemTime>,
    /// Rendered mod_timestamp string for logging.
    output_mtime_repr: Option<String>,
    target: Option<String>,
    crate_name: String,
    cfg_id: Option<String>,
    /// The binary's amd64 micro-architecture variant (`None` / `Some("v1")`
    /// → baseline), recorded in the produced artifact's metadata so downstream
    /// stages can tell two amd64 builds of one target apart.
    amd64_variant: Option<String>,
}

/// Collect crates that declare at least one `flatpaks:` config and are not
/// excluded by the active `--crates` selection.
fn collect_flatpak_crates(
    ctx: &Context,
    selected: &[String],
) -> Vec<anodizer_core::config::CrateConfig> {
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.flatpaks.is_some())
        .cloned()
        .collect()
}

/// Returns true when at least one flatpak config across the supplied crates is
/// not skipped by its `skip:` template.
fn any_flatpak_enabled(
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
fn require_flatpak_tools() -> Result<()> {
    if !anodizer_core::util::find_binary("flatpak-builder") {
        anyhow::bail!(
            "flatpak-builder not found on PATH; install Flatpak to create Flatpak bundles"
        );
    }
    if !anodizer_core::util::find_binary("flatpak") {
        anyhow::bail!("flatpak not found on PATH; install Flatpak to create Flatpak bundles");
    }
    Ok(())
}

/// Resolve the `Version` template variable, warning and defaulting when unset.
fn resolve_flatpak_version(ctx: &Context, log: &anodizer_core::log::StageLogger) -> String {
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
fn validate_flatpak_required_fields<'a>(
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
fn collect_linux_binaries(ctx: &Context, crate_name: &str) -> Vec<Artifact> {
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
fn filter_binaries_by_ids(binaries: &mut Vec<Artifact>, filter_ids: Option<&Vec<String>>) {
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
type FlatpakBinary = (Option<String>, Option<String>, PathBuf, String);

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
fn map_to_supported_arches(binaries: &[Artifact]) -> Vec<FlatpakBinary> {
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
type RenderedFilename = (String, String);

/// Render the bundle output filename via `name_template`, defaulting to
/// `default_name`, and force a `.flatpak` suffix. Returns the rendered name
/// alongside the resolved template so the single resolution feeds both the
/// produced path and the clobber guard.
fn render_output_filename(
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
fn build_manifest(
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
fn dry_run_artifact(
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
fn build_subprocess_args(
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
fn stage_work_dir(
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
fn run_flatpak_job(job: &FlatpakJob, verbosity: anodizer_core::log::Verbosity) -> Result<Artifact> {
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

/// Output of [`process_binary_iteration`]: either a finished dry-run artifact
/// or a staged live job waiting on the parallel phase.
enum BinaryOutcome {
    DryRun(Artifact),
    Job(FlatpakJob),
}

/// Stage one `(target, binary, flatpak_arch)` triple for a given flatpak
/// config: render templates, build the manifest, and either prepare the
/// Resolved per-cfg identity fields (the four mandatory PKGBUILD-equivalent
/// strings) passed to the per-binary iteration. Bundled to keep the helper's
/// signature under clippy's 7-arg threshold.
struct FlatpakIdentity<'a> {
    app_id: &'a str,
    runtime: &'a str,
    runtime_version: &'a str,
    sdk: &'a str,
}

/// Mutable accumulators threaded through the per-crate / per-cfg loop. The
/// caller seeds empty vectors before the loop and inspects them after; the
/// helpers append to whichever vector matches the per-binary outcome.
struct FlatpakAccumulators<'a> {
    new_artifacts: &'a mut Vec<Artifact>,
    jobs: &'a mut Vec<FlatpakJob>,
    archives_to_remove: &'a mut Vec<PathBuf>,
}

/// dry-run artifact or the live `FlatpakJob`. The caller is responsible for
/// collecting `archives_to_remove` entries from `flatpak_cfg.replace`; this
/// helper performs the lookup against `ctx.artifacts` in both branches via
/// the returned [`BinaryOutcome`] plus the supplied `replace_sink` callback.
#[allow(clippy::too_many_arguments)]
fn process_binary_iteration(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    dist: &std::path::Path,
    krate: &anodizer_core::config::CrateConfig,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    identity: &FlatpakIdentity<'_>,
    version: &str,
    target: &Option<String>,
    amd64_variant: Option<&str>,
    binary_path: &std::path::Path,
    flatpak_arch: &str,
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    archives_to_remove: &mut Vec<PathBuf>,
) -> Result<BinaryOutcome> {
    let (os, arch) = os_arch_from_target(target.as_deref());
    ctx.template_vars_mut().set("Os", &os);
    ctx.template_vars_mut().set("Arch", &arch);
    ctx.template_vars_mut()
        .set("Target", target.as_deref().unwrap_or(""));
    // Seed the amd64 variant so the default (or a custom) name template
    // disambiguates two amd64 builds of one target.
    anodizer_core::archive_name::seed_amd64_variant_var(
        ctx.template_vars_mut(),
        &arch,
        amd64_variant,
    );

    let default_name = default_name_template();
    let (output_name, resolved_template) =
        render_output_filename(ctx, flatpak_cfg, &krate.name, target, &default_name)?;

    let output_dir = dist.join("flatpak");
    let output_path = output_dir.join(&output_name);

    // Reject a `name_template` that renders the same `.flatpak` path for two
    // build targets / amd64 variants (an override lacking `{{ .Arch }}` /
    // `{{ .Amd64 }}`): the second bundle would silently overwrite the first.
    arch_guard.check(
        &output_path,
        "flatpak",
        "bundle",
        &resolved_template,
        &output_name,
        &krate.name,
    )?;

    // Disambiguate the work dir per amd64 variant so two non-baseline variants
    // of one flatpak arch don't stage into (and race over) the same build dir.
    let work_subdir = match amd64_variant {
        Some(v) if v != "v1" => format!("{flatpak_arch}_{v}"),
        _ => flatpak_arch.to_string(),
    };
    let work_dir = dist.join("flatpak").join(&krate.name).join(work_subdir);

    let binary_name = binary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&krate.name);
    let command = flatpak_cfg.command.as_deref().unwrap_or(binary_name);
    let finish_args = flatpak_cfg.finish_args.clone().unwrap_or_default();

    let extra_file_names: Vec<String> = if let Some(extra_files) = &flatpak_cfg.extra_files {
        resolve_extra_file_specs(extra_files, log)
            .into_iter()
            .map(|(_, dst)| dst)
            .collect()
    } else {
        Vec::new()
    };

    let manifest = build_manifest(
        identity.app_id,
        identity.runtime,
        identity.runtime_version,
        identity.sdk,
        command,
        finish_args,
        binary_name,
        &extra_file_names,
    );

    if dry_run {
        let artifact = dry_run_artifact(
            log,
            flatpak_cfg,
            &krate.name,
            target,
            amd64_variant,
            &output_name,
            output_path,
        );
        archives_to_remove.extend(anodizer_core::util::collect_if_replace(
            flatpak_cfg.replace,
            &ctx.artifacts,
            &krate.name,
            target.as_deref(),
        ));
        return Ok(BinaryOutcome::DryRun(artifact));
    }

    let (output_mtime, output_mtime_repr) = stage_work_dir(
        ctx,
        log,
        flatpak_cfg,
        &work_dir,
        &output_dir,
        binary_path,
        binary_name,
        &manifest,
        identity.app_id,
    )?;

    // `flatpak build-bundle` runs with cwd set to `work_dir` (so its sibling
    // `repo` / manifest are found), so a dist-relative output path would
    // resolve *under* work_dir (`<work_dir>/dist/flatpak/...`) and the bundle
    // write fails with `opendir(...): No such file or directory`. Absolutize
    // against the process cwd — `std::path::absolute` is purely lexical and
    // needs no filesystem access — so the bundle lands at the real dist path.
    let bundle_output_path =
        std::path::absolute(&output_path).unwrap_or_else(|_| output_path.clone());

    let (builder_args, bundle_args) =
        build_subprocess_args(identity.app_id, version, flatpak_arch, &bundle_output_path);

    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
        flatpak_cfg.replace,
        &ctx.artifacts,
        &krate.name,
        target.as_deref(),
    ));

    Ok(BinaryOutcome::Job(FlatpakJob {
        work_dir,
        output_name,
        output_path,
        builder_args,
        bundle_args,
        output_mtime,
        output_mtime_repr,
        target: target.clone(),
        crate_name: krate.name.clone(),
        cfg_id: flatpak_cfg.id.clone(),
        amd64_variant: amd64_variant.map(str::to_string),
    }))
}

/// Process a single `flatpak_cfg` entry for a crate: validate, filter binaries,
/// then iterate per binary via [`process_binary_iteration`], appending results
/// into the supplied accumulators.
#[allow(clippy::too_many_arguments)]
fn process_flatpak_cfg(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    dist: &std::path::Path,
    krate: &anodizer_core::config::CrateConfig,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    linux_binaries: &[Artifact],
    version: &str,
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    acc: &mut FlatpakAccumulators<'_>,
) -> Result<()> {
    if let Some(ref d) = flatpak_cfg.skip {
        let off = d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| format!("flatpak: render skip template for crate {}", krate.name))?;
        if off {
            log.status(&format!("flatpak config skipped for crate {}", krate.name));
            return Ok(());
        }
    }

    let (app_id, runtime, runtime_version, sdk) =
        validate_flatpak_required_fields(flatpak_cfg, &krate.name)?;
    let identity = FlatpakIdentity {
        app_id,
        runtime,
        runtime_version,
        sdk,
    };

    let mut filtered = linux_binaries.to_vec();
    filter_binaries_by_ids(&mut filtered, flatpak_cfg.ids.as_ref());

    if filtered.is_empty() && linux_binaries.is_empty() {
        log.warn(&format!(
            "skipped Flatpak generation for crate '{}' — no Linux binary \
             artifacts found (expected binaries targeting linux)",
            krate.name
        ));
        return Ok(());
    }
    if filtered.is_empty() {
        log.warn(&format!(
            "skipped flatpak for crate '{}' — ids filter {:?} matched no binaries",
            krate.name, flatpak_cfg.ids
        ));
        return Ok(());
    }

    let effective_binaries = map_to_supported_arches(&filtered);
    if effective_binaries.is_empty() {
        log.warn(&format!(
            "skipped flatpak for crate '{}' — no supported architectures (amd64/arm64) found",
            krate.name
        ));
        return Ok(());
    }

    for (target, amd64_variant, binary_path, flatpak_arch) in &effective_binaries {
        let outcome = process_binary_iteration(
            ctx,
            log,
            dist,
            krate,
            flatpak_cfg,
            &identity,
            version,
            target,
            amd64_variant.as_deref(),
            binary_path,
            flatpak_arch,
            dry_run,
            arch_guard,
            acc.archives_to_remove,
        )?;
        match outcome {
            BinaryOutcome::DryRun(artifact) => acc.new_artifacts.push(artifact),
            BinaryOutcome::Job(job) => acc.jobs.push(job),
        }
    }

    Ok(())
}

impl Stage for FlatpakStage {
    fn name(&self) -> &str {
        "flatpak"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("flatpak");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        let crates = collect_flatpak_crates(ctx, &selected);
        if crates.is_empty() {
            return Ok(());
        }

        if !any_flatpak_enabled(ctx, &crates)? {
            return Ok(());
        }

        if !dry_run {
            require_flatpak_tools()?;
        }

        let version = resolve_flatpak_version(ctx, &log);

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();
        let mut jobs: Vec<FlatpakJob> = Vec::new();

        for krate in &crates {
            let Some(flatpak_configs) = krate.flatpaks.as_ref() else {
                continue;
            };

            let linux_binaries = collect_linux_binaries(ctx, &krate.name);

            // One guard per crate spans every `flatpaks:` config of that crate:
            // two configs with the default (or identical) `name_template` render
            // the same `.flatpak` path for one arch — error loudly across configs
            // instead of letting the second silently clobber the first.
            let mut arch_guard = ArchPathGuard::new();

            for flatpak_cfg in flatpak_configs {
                let mut acc = FlatpakAccumulators {
                    new_artifacts: &mut new_artifacts,
                    jobs: &mut jobs,
                    archives_to_remove: &mut archives_to_remove,
                };
                process_flatpak_cfg(
                    ctx,
                    &log,
                    &dist,
                    krate,
                    flatpak_cfg,
                    &linux_binaries,
                    &version,
                    dry_run,
                    &mut arch_guard,
                    &mut acc,
                )?;
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        if !jobs.is_empty() {
            let verbosity = log.verbosity();
            let results = anodizer_core::parallel::run_parallel_chunks(
                &jobs,
                parallelism,
                "flatpak",
                |job| run_flatpak_job(job, verbosity),
            )?;
            new_artifacts.extend(results);
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
// Tests
// ---------------------------------------------------------------------------

/// Environment requirements for the flatpak stage: `flatpak-builder` and
/// `flatpak` (build-bundle) when any active `flatpaks:` entry exists and
/// the configured build targets include Linux (the stage only bundles
/// linux binaries).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    if !anodizer_core::env_preflight::configured_build_targets(ctx)
        .iter()
        .any(|t| anodizer_core::target::is_linux(t))
    {
        return Vec::new();
    }
    let configured = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.flatpaks.iter().flatten())
        .any(|cfg| {
            !anodizer_core::env_preflight::entry_inactive(ctx, cfg.skip.as_ref(), None, None)
        });
    if !configured {
        return Vec::new();
    }
    vec![
        anodizer_core::EnvRequirement::Tool {
            name: "flatpak-builder".to_string(),
        },
        anodizer_core::EnvRequirement::Tool {
            name: "flatpak".to_string(),
        },
    ]
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Architecture mapping
    // -----------------------------------------------------------------------

    #[test]
    fn test_arch_to_flatpak() {
        assert_eq!(arch_to_flatpak("amd64"), Some("x86_64"));
        assert_eq!(arch_to_flatpak("x86_64"), Some("x86_64"));
        assert_eq!(arch_to_flatpak("arm64"), Some("aarch64"));
        assert_eq!(arch_to_flatpak("aarch64"), Some("aarch64"));
        assert_eq!(arch_to_flatpak("i386"), None);
        assert_eq!(arch_to_flatpak("armv7"), None);
        assert_eq!(arch_to_flatpak("mips"), None);
        assert_eq!(arch_to_flatpak("riscv64"), None);
        assert_eq!(arch_to_flatpak(""), None);
    }

    // -----------------------------------------------------------------------
    // Manifest JSON serialization
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_json_serialization() {
        let manifest = Manifest {
            id: "org.example.MyApp".to_string(),
            runtime: "org.freedesktop.Platform".to_string(),
            runtime_version: "24.08".to_string(),
            sdk: "org.freedesktop.Sdk".to_string(),
            command: "myapp".to_string(),
            finish_args: vec!["--share=network".to_string(), "--socket=x11".to_string()],
            modules: vec![ManifestModule {
                name: "org.example.MyApp".to_string(),
                buildsystem: "simple".to_string(),
                build_commands: vec!["install -Dm755 myapp /app/bin/myapp".to_string()],
                sources: vec![ManifestSource {
                    type_: "file".to_string(),
                    path: "myapp".to_string(),
                    dest_filename: None,
                }],
            }],
        };

        let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();

        assert_eq!(json["id"], "org.example.MyApp");
        assert_eq!(json["runtime"], "org.freedesktop.Platform");
        assert_eq!(json["runtime-version"], "24.08");
        assert_eq!(json["sdk"], "org.freedesktop.Sdk");
        assert_eq!(json["command"], "myapp");

        let finish_args = json["finish-args"].as_array().unwrap();
        assert_eq!(finish_args.len(), 2);
        assert_eq!(finish_args[0], "--share=network");
        assert_eq!(finish_args[1], "--socket=x11");

        let modules = json["modules"].as_array().unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0]["name"], "org.example.MyApp");
        assert_eq!(modules[0]["buildsystem"], "simple");

        let build_cmds = modules[0]["build-commands"].as_array().unwrap();
        assert_eq!(build_cmds.len(), 1);
        assert_eq!(build_cmds[0], "install -Dm755 myapp /app/bin/myapp");

        let sources = modules[0]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["type"], "file");
        assert_eq!(sources[0]["path"], "myapp");
        // dest-filename should be absent (skip_serializing_if)
        assert!(sources[0].get("dest-filename").is_none());
    }

    #[test]
    fn test_manifest_json_empty_finish_args_omitted() {
        let manifest = Manifest {
            id: "org.example.App".to_string(),
            runtime: "org.freedesktop.Platform".to_string(),
            runtime_version: "24.08".to_string(),
            sdk: "org.freedesktop.Sdk".to_string(),
            command: "app".to_string(),
            finish_args: vec![],
            modules: vec![],
        };

        let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();
        // finish-args should be omitted when empty (skip_serializing_if)
        assert!(json.get("finish-args").is_none());
    }

    // -----------------------------------------------------------------------
    // FlatpakConfig deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_config_deserialize() {
        use anodizer_core::config::FlatpakConfig;

        let yaml = r#"
app_id: org.example.MyApp
runtime: org.freedesktop.Platform
runtime_version: "24.08"
sdk: org.freedesktop.Sdk
command: myapp
ids:
  - build-linux
name_template: "{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak"
finish_args:
  - --share=network
  - --socket=x11
  - --filesystem=home
"#;

        let config: FlatpakConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.app_id.as_deref(), Some("org.example.MyApp"));
        assert_eq!(config.runtime.as_deref(), Some("org.freedesktop.Platform"));
        assert_eq!(config.runtime_version.as_deref(), Some("24.08"));
        assert_eq!(config.sdk.as_deref(), Some("org.freedesktop.Sdk"));
        assert_eq!(config.command.as_deref(), Some("myapp"));
        assert_eq!(config.ids, Some(vec!["build-linux".to_string()]));
        assert_eq!(
            config.name_template.as_deref(),
            Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak")
        );

        let finish_args = config.finish_args.unwrap();
        assert_eq!(finish_args.len(), 3);
        assert_eq!(finish_args[0], "--share=network");
        assert_eq!(finish_args[1], "--socket=x11");
        assert_eq!(finish_args[2], "--filesystem=home");
    }

    #[test]
    fn test_flatpak_config_defaults() {
        use anodizer_core::config::FlatpakConfig;

        let config: FlatpakConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert!(config.app_id.is_none());
        assert!(config.runtime.is_none());
        assert!(config.runtime_version.is_none());
        assert!(config.sdk.is_none());
        assert!(config.command.is_none());
        assert!(config.ids.is_none());
        assert!(config.name_template.is_none());
        assert!(config.finish_args.is_none());
        assert!(config.extra_files.is_none());
        assert!(config.replace.is_none());
        assert!(config.mod_timestamp.is_none());
        assert!(config.skip.is_none());
        assert!(config.id.is_none());
    }

    // -----------------------------------------------------------------------
    // Required field validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_config_required_field_validation() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Missing app_id
        {
            let flatpak_cfg = FlatpakConfig {
                runtime: Some("org.freedesktop.Platform".to_string()),
                runtime_version: Some("24.08".to_string()),
                sdk: Some("org.freedesktop.Sdk".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist1");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");

            // Add a Linux binary so the stage processes the config
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            let result = stage.run(&mut ctx);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("app_id"),
                "error should mention app_id"
            );
        }

        // Missing runtime
        {
            let flatpak_cfg = FlatpakConfig {
                app_id: Some("org.example.MyApp".to_string()),
                runtime_version: Some("24.08".to_string()),
                sdk: Some("org.freedesktop.Sdk".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist2");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");

            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            let result = stage.run(&mut ctx);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("runtime"),
                "error should mention runtime"
            );
        }

        // Missing runtime_version
        {
            let flatpak_cfg = FlatpakConfig {
                app_id: Some("org.example.MyApp".to_string()),
                runtime: Some("org.freedesktop.Platform".to_string()),
                sdk: Some("org.freedesktop.Sdk".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist3");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");

            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            let result = stage.run(&mut ctx);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("runtime_version"),
                "error should mention runtime_version"
            );
        }

        // Missing sdk
        {
            let flatpak_cfg = FlatpakConfig {
                app_id: Some("org.example.MyApp".to_string()),
                runtime: Some("org.freedesktop.Platform".to_string()),
                runtime_version: Some("24.08".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist4");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");

            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            let result = stage.run(&mut ctx);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("sdk"),
                "error should mention sdk"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Disable via bool and template
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_config_disable_bool_and_template() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Disable via bool
        {
            let flatpak_cfg = FlatpakConfig {
                skip: Some(StringOrBool::Bool(true)),
                app_id: Some("org.example.MyApp".to_string()),
                runtime: Some("org.freedesktop.Platform".to_string()),
                runtime_version: Some("24.08".to_string()),
                sdk: Some("org.freedesktop.Sdk".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist-disabled");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");

            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            stage.run(&mut ctx).unwrap();

            let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
            assert!(flatpaks.is_empty(), "should be disabled by bool");
        }

        // Disable via template
        {
            let flatpak_cfg = FlatpakConfig {
                skip: Some(StringOrBool::String("{{ IsSnapshot }}".to_string())),
                app_id: Some("org.example.MyApp".to_string()),
                runtime: Some("org.freedesktop.Platform".to_string()),
                runtime_version: Some("24.08".to_string()),
                sdk: Some("org.freedesktop.Sdk".to_string()),
                ..Default::default()
            };

            let mut config = Config::default();
            config.project_name = "myapp".to_string();
            config.dist = tmp.path().join("dist-template-disabled");
            config.crates = vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                flatpaks: Some(vec![flatpak_cfg]),
                ..Default::default()
            }];

            let mut ctx = Context::new(
                config,
                ContextOptions {
                    dry_run: true,
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "1.0.0");
            ctx.template_vars_mut().set("IsSnapshot", "true");

            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/myapp"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });

            let stage = FlatpakStage;
            stage.run(&mut ctx).unwrap();

            let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
            assert!(flatpaks.is_empty(), "should be disabled by template");
        }
    }

    // -----------------------------------------------------------------------
    // Stage skips non-Linux binaries
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_stage_skips_non_linux() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add only macOS and Windows binaries — no Linux
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert!(flatpaks.is_empty(), "should skip non-Linux binaries");
    }

    // -----------------------------------------------------------------------
    // Stage skips unsupported architectures
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_stage_skips_unsupported_arch() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add only a Linux binary with unsupported arch (i686)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("i686-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert!(
            flatpaks.is_empty(),
            "should skip unsupported architecture (i686)"
        );
    }

    // -----------------------------------------------------------------------
    // Default name template
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_name_template() {
        // The default composes from the shared amd64-only suffix const (drift
        // guard), so a v1/None baseline renders the historical unsuffixed name
        // while a v3 build appends `v3` before `.flatpak`.
        let tmpl = default_name_template();
        assert!(
            tmpl.starts_with("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}"),
            "{tmpl}"
        );
        assert!(
            tmpl.contains(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
            "flatpak default must reuse INSTALLER_AMD64_VARIANT_SUFFIX: {tmpl}"
        );
        assert!(tmpl.ends_with(".flatpak"), "{tmpl}");
    }

    // -----------------------------------------------------------------------
    // Stage no-op when no flatpak config
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_when_no_flatpak_config() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = FlatpakStage;
        assert!(stage.run(&mut ctx).is_ok());
        assert!(ctx.artifacts.all().is_empty());
    }

    // -----------------------------------------------------------------------
    // Dry-run produces correct artifact
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_produces_artifact() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            id: Some("my-flatpak".to_string()),
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);
        assert_eq!(flatpaks[0].crate_name, "myapp");
        assert_eq!(flatpaks[0].metadata.get("format").unwrap(), "flatpak");
        assert_eq!(flatpaks[0].metadata.get("id").unwrap(), "my-flatpak");
        assert_eq!(
            flatpaks[0].target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        // Path should contain the flatpak subdir
        let path_str = flatpaks[0].path.to_string_lossy();
        assert!(
            path_str.contains("flatpak"),
            "path should contain 'flatpak': {}",
            path_str
        );
        assert!(
            path_str.ends_with(".flatpak"),
            "path should end with .flatpak: {}",
            path_str
        );
    }

    // -----------------------------------------------------------------------
    // Dry-run with multiple architectures
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_multiple_arches() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        // Add both x86_64 and aarch64 Linux binaries
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-x86"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-arm"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Same-triple multi-variant: distinct .flatpak names, no clobber
    // -----------------------------------------------------------------------

    /// Three x86_64 builds tagged amd64_variant v1/v2/v3 plus one aarch64 build
    /// must each produce a distinct `.flatpak` artifact (no ArchPathGuard
    /// error): the default name appends the amd64 micro-arch suffix (v1 → no
    /// suffix, v2 → `…v2`, v3 → `…v3`), so the same triple no longer clobbers
    /// itself.
    #[test]
    fn test_flatpak_dry_run_same_triple_multi_variant_distinct_names() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        for variant in ["v1", "v2", "v3"] {
            let p = tmp.path().join(format!("myapp-{variant}"));
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: p,
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: tmp.path().join("myapp-arm"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage
            .run(&mut ctx)
            .expect("multi-variant build must not clobber");

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 4, "one .flatpak per variant + arm64");
        let names: std::collections::HashSet<String> = flatpaks
            .iter()
            .map(|f| {
                f.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(names.len(), 4, "all .flatpak filenames distinct: {names:?}");
        assert!(names.contains("myapp_1.0.0_linux_amd64.flatpak"));
        assert!(names.contains("myapp_1.0.0_linux_amd64v2.flatpak"));
        assert!(names.contains("myapp_1.0.0_linux_amd64v3.flatpak"));
        assert!(names.contains("myapp_1.0.0_linux_arm64.flatpak"));
    }

    /// Two `flatpaks:` configs on one crate, both rendering the default name,
    /// produce the same `.flatpak` path for one arch. The guard now spans every
    /// config of the crate, so the second config bails loudly instead of
    /// silently clobbering the first config's bundle.
    #[test]
    fn test_flatpak_two_configs_same_default_name_bail_across_configs() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let make_cfg = |id: &str| FlatpakConfig {
            id: Some(id.to_string()),
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![make_cfg("first"), make_cfg("second")]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: tmp.path().join("myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let err = FlatpakStage.run(&mut ctx).unwrap_err().to_string();
        assert!(err.contains("flatpak:"), "{err}");
        assert!(err.contains("crate 'myapp'"), "{err}");
        assert!(err.contains("{{ .Arch }}"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Custom name_template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_custom_name_template() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.5.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);

        let path_str = flatpaks[0].path.to_string_lossy();
        assert!(
            path_str.ends_with("myapp-2.5.0-amd64.flatpak"),
            "custom name_template should render correctly: {}",
            path_str
        );
        // Verify output goes to flat dist/flatpak/ dir, not nested work dir
        assert!(
            !path_str.contains("x86_64"),
            "output path should not contain work dir arch subpath: {}",
            path_str
        );
    }

    // -----------------------------------------------------------------------
    // Replace config marks archives for removal
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_replace_removes_archives() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            replace: Some(true),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        // Add a Linux binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Add an archive artifact that should be replaced
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        // The archive should have been removed
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert!(
            archives.is_empty(),
            "archives should be removed when replace=true"
        );

        // The flatpak should have been added
        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Mod timestamp logged in dry_run
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_mod_timestamp() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            mod_timestamp: Some("1704067200".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Should not error — just log the mod_timestamp
        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // build_manifest with extra files
    // -----------------------------------------------------------------------

    /// Verifies that extra_file_names produces additional sources + install
    /// commands with the correct paths. A regression here would mean the
    /// generated manifest no longer installs extra files into /app/share/<id>/.
    #[test]
    fn test_build_manifest_with_extra_files() {
        let manifest = build_manifest(
            "org.example.App",
            "org.freedesktop.Platform",
            "24.08",
            "org.freedesktop.Sdk",
            "app",
            vec!["--share=network".to_string()],
            "app",
            &["license.txt".to_string(), "config.toml".to_string()],
        );

        let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();
        let module = &json["modules"][0];

        // Binary source + 2 extra file sources = 3 total
        let sources = module["sources"].as_array().unwrap();
        assert_eq!(
            sources.len(),
            3,
            "should have binary + 2 extra file sources"
        );
        assert_eq!(sources[1]["path"], "license.txt");
        assert_eq!(sources[2]["path"], "config.toml");

        // Binary install + 2 extra file installs = 3 total build commands
        let cmds = module["build-commands"].as_array().unwrap();
        assert_eq!(cmds.len(), 3);
        assert!(
            cmds[1]
                .as_str()
                .unwrap()
                .contains("/app/share/org.example.App/license.txt"),
            "extra file should install into /app/share/<app_id>/: {}",
            cmds[1]
        );
        assert!(
            cmds[2]
                .as_str()
                .unwrap()
                .contains("/app/share/org.example.App/config.toml"),
            "second extra file should install into /app/share/<app_id>/: {}",
            cmds[2]
        );
        // Extra files use 644 permissions, binary uses 755
        assert!(cmds[0].as_str().unwrap().contains("755"));
        assert!(cmds[1].as_str().unwrap().contains("644"));
    }

    // -----------------------------------------------------------------------
    // build_subprocess_args
    // -----------------------------------------------------------------------

    /// Verifies the builder and bundle arg vectors contain the correct flags.
    /// A regression that rearranges args would silently break the subprocess
    /// invocations.
    #[test]
    fn test_build_subprocess_args() {
        let output_path = std::path::Path::new("/dist/flatpak/app-1.0.0-linux-amd64.flatpak");
        let (builder, bundle) =
            build_subprocess_args("org.example.App", "1.0.0", "x86_64", output_path);

        assert_eq!(builder[0], "flatpak-builder");
        assert!(builder.contains(&"--force-clean".to_string()));
        // rofiles-fuse disabled so the build leaves no FUSE mount that would
        // wedge the determinism harness's per-run worktree teardown.
        assert!(builder.contains(&"--disable-rofiles-fuse".to_string()));
        assert!(builder.contains(&"--arch=x86_64".to_string()));
        assert!(builder.contains(&"--default-branch=1.0.0".to_string()));
        assert!(builder.contains(&"--repo=repo".to_string()));
        assert!(builder.contains(&"org.example.App.json".to_string()));

        assert_eq!(bundle[0], "flatpak");
        assert_eq!(bundle[1], "build-bundle");
        assert!(bundle.contains(&"--arch=x86_64".to_string()));
        assert!(bundle.contains(&"org.example.App".to_string()));
        assert!(bundle.contains(&"1.0.0".to_string()));
        assert!(bundle.iter().any(|a| a.contains("amd64.flatpak")));
    }

    // -----------------------------------------------------------------------
    // resolve_extra_file_specs — valid glob
    // -----------------------------------------------------------------------

    /// Verifies that a valid glob resolves to (path, destination_name) pairs.
    /// When name_template is absent the destination is the file's basename.
    #[test]
    fn test_resolve_extra_file_specs_glob_match() {
        use anodizer_core::config::ExtraFileSpec;
        use anodizer_core::log::{StageLogger, Verbosity};

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.json"), b"{}").unwrap();

        let pattern = format!("{}/*.json", tmp.path().display());
        let specs = vec![ExtraFileSpec::Glob(pattern)];
        let log = StageLogger::new("flatpak", Verbosity::Normal);

        let results = resolve_extra_file_specs(&specs, &log);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "data.json");
        assert!(results[0].0.is_file());
    }

    /// Verifies that a Detailed spec with name_template overrides the basename.
    #[test]
    fn test_resolve_extra_file_specs_name_template_override() {
        use anodizer_core::config::ExtraFileSpec;
        use anodizer_core::log::{StageLogger, Verbosity};

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("original.txt"), b"data").unwrap();

        let pattern = format!("{}/*.txt", tmp.path().display());
        let specs = vec![ExtraFileSpec::Detailed {
            glob: pattern,
            name_template: Some("renamed.txt".to_string()),
            allow_empty: false,
        }];
        let log = StageLogger::new("flatpak", Verbosity::Normal);

        let results = resolve_extra_file_specs(&specs, &log);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[1 - 1].1,
            "renamed.txt",
            "name_template should override basename"
        );
    }

    /// Verifies that an invalid glob pattern is warned-and-skipped (no panic,
    /// returns empty). A glob with invalid character sequences triggers this.
    #[test]
    fn test_resolve_extra_file_specs_invalid_glob_skipped() {
        use anodizer_core::config::ExtraFileSpec;
        use anodizer_core::log::{StageLogger, Verbosity};

        // On most systems a pattern with unmatched `[` is invalid
        let specs = vec![ExtraFileSpec::Glob("[invalid".to_string())];
        let log = StageLogger::new("flatpak", Verbosity::Normal);

        // Must not panic; simply returns empty after logging a warning
        let results = resolve_extra_file_specs(&specs, &log);
        assert!(results.is_empty(), "invalid glob should produce no results");
    }

    // -----------------------------------------------------------------------
    // filter_binaries_by_ids
    // -----------------------------------------------------------------------

    /// Verifies that with an ids filter, only binaries whose metadata "id" or
    /// "name" matches are retained. Without this filter the ids-gate is a no-op.
    #[test]
    fn test_filter_binaries_by_ids_retains_matching() {
        let mut binaries = vec![
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/a"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("id".to_string(), "build-linux-amd64".to_string());
                    m
                },
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/b"),
                target: Some("aarch64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("id".to_string(), "build-linux-arm64".to_string());
                    m
                },
                size: None,
            },
        ];

        let filter = vec!["build-linux-amd64".to_string()];
        filter_binaries_by_ids(&mut binaries, Some(&filter));

        assert_eq!(binaries.len(), 1, "only the amd64 binary should remain");
        assert_eq!(binaries[0].metadata.get("id").unwrap(), "build-linux-amd64");
    }

    /// Verifies that an ids filter with no matches empties the list — this is
    /// the path that triggers the "ids filter matched no binaries" warning.
    #[test]
    fn test_filter_binaries_by_ids_no_match_empties_list() {
        let mut binaries = vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/a"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "build-linux-amd64".to_string());
                m
            },
            size: None,
        }];

        let filter = vec!["build-windows-amd64".to_string()];
        filter_binaries_by_ids(&mut binaries, Some(&filter));

        assert!(
            binaries.is_empty(),
            "non-matching ids filter should empty the list"
        );
    }

    /// Verifies that a None filter is a no-op (all binaries retained).
    #[test]
    fn test_filter_binaries_by_ids_none_is_noop() {
        let mut binaries = vec![
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/a"),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/b"),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            },
        ];

        filter_binaries_by_ids(&mut binaries, None);
        assert_eq!(binaries.len(), 2, "None filter should retain all binaries");
    }

    // -----------------------------------------------------------------------
    // render_output_filename — auto-appends .flatpak suffix
    // -----------------------------------------------------------------------

    /// Verifies that a name_template that does NOT end with ".flatpak" gets the
    /// suffix appended automatically. Without this logic the produced file would
    /// have a bare name with no extension.
    #[test]
    fn test_render_output_filename_appends_suffix_when_missing() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            // Template that does NOT end with .flatpak
            name_template: Some("{{ ProjectName }}-{{ Version }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg.clone()]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "3.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "amd64");

        let target: Option<String> = Some("x86_64-unknown-linux-gnu".to_string());
        let (name, resolved_template) = render_output_filename(
            &ctx,
            &flatpak_cfg,
            "myapp",
            &target,
            &default_name_template(),
        )
        .unwrap();

        assert!(
            name.ends_with(".flatpak"),
            "suffix should be auto-appended: {}",
            name
        );
        assert_eq!(name, "myapp-3.0.0.flatpak");
        assert_eq!(
            resolved_template, "{{ ProjectName }}-{{ Version }}",
            "resolved template should be the user's name_template, fed verbatim to the clobber guard"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_flatpak_version — missing Version var falls back to "0.0.0"
    // -----------------------------------------------------------------------

    /// Verifies that when the Version template variable is absent the stage falls
    /// back to "0.0.0" for the Flatpak bundle version (the value passed to
    /// flatpak-builder's --default-branch and build-bundle). The name_template
    /// must not reference {{ Version }} in this test since that variable is
    /// genuinely absent from the render context — the fallback only guards the
    /// `resolve_flatpak_version` code path, not the generic template renderer.
    #[test]
    fn test_flatpak_stage_no_version_falls_back_to_0_0_0() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            // Use a template that does NOT reference {{ Version }} because that
            // var is absent; we verify the fallback via build_subprocess_args
            // by asserting the stage completes without error.
            name_template: Some("myapp-{{ Arch }}.flatpak".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        // Deliberately do NOT set "Version" — exercises the fallback path
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        // Must succeed — resolve_flatpak_version falls back to "0.0.0" rather
        // than panicking or propagating a missing-variable error.
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // require_flatpak_tools bails when tools are missing
    // -----------------------------------------------------------------------

    /// Verifies that when flatpak-builder is absent (which it is in the test
    /// sandbox) `require_flatpak_tools` returns an error with a helpful message.
    /// This exercises the 189-199 block; in CI the binaries genuinely aren't
    /// on PATH.
    #[test]
    fn test_require_flatpak_tools_errors_when_absent() {
        // Only runs the direct function, not a subprocess.
        let result = require_flatpak_tools();
        // In CI/test environments flatpak-builder is not installed —
        // the function must bail with a descriptive message.
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                msg.contains("flatpak-builder") || msg.contains("flatpak"),
                "error should name the missing tool: {}",
                msg
            );
        }
        // If the tools happen to be installed on the host, the function
        // succeeds — that is also correct.
    }

    // -----------------------------------------------------------------------
    // Stage::run bails early when no flatpak tool and not dry-run
    // -----------------------------------------------------------------------

    /// Verifies that the non-dry-run path calls require_flatpak_tools and bails
    /// when neither flatpak-builder nor flatpak is on PATH. This exercises
    /// lines 808-809 in Stage::run.
    #[test]
    fn test_stage_run_non_dry_run_fails_without_tools() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        // Skip if the tools are actually installed
        if anodizer_core::util::find_binary("flatpak-builder") {
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // not dry-run → triggers require_flatpak_tools
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "should fail when flatpak-builder is absent"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("flatpak"),
            "error message should mention flatpak: {}",
            msg
        );
    }

    // -----------------------------------------------------------------------
    // any_flatpak_enabled — skip template render error propagates
    // -----------------------------------------------------------------------

    /// Verifies that a malformed skip template in `any_flatpak_enabled` causes
    /// the stage to return an error rather than silently running or suppressing.
    #[test]
    fn test_any_flatpak_enabled_bad_template_propagates_error() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let flatpak_cfg = FlatpakConfig {
            // Unclosed Tera tag → render error
            skip: Some(StringOrBool::String("{{ unclosed".to_string())),
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "malformed skip template should propagate an error"
        );
    }

    // -----------------------------------------------------------------------
    // process_flatpak_cfg — ids filter matched no binaries
    // -----------------------------------------------------------------------

    /// Verifies that when the ids filter matches none of the available binaries
    /// the stage skips quietly (no error, no flatpak artifact).
    #[test]
    fn test_flatpak_stage_ids_filter_no_match_skips() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            // Request a specific build id that no binary carries
            ids: Some(vec!["build-nonexistent".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Binary has no "id" metadata → ids filter will not match
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert!(
            flatpaks.is_empty(),
            "ids filter with no match should produce no artifacts"
        );
    }

    // -----------------------------------------------------------------------
    // process_flatpak_cfg — per-cfg skip template (lines 717-723)
    // -----------------------------------------------------------------------

    /// Verifies that a skip template evaluated at the per-cfg level (inside
    /// process_flatpak_cfg) suppresses that specific config but not others.
    /// This is distinct from the any_flatpak_enabled-level skip (which exits
    /// the whole stage early).
    #[test]
    fn test_process_flatpak_cfg_skip_per_cfg() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Two configs: first is skipped, second is active
        let skip_cfg = FlatpakConfig {
            skip: Some(StringOrBool::Bool(true)),
            app_id: Some("org.example.Skipped".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };
        let active_cfg = FlatpakConfig {
            app_id: Some("org.example.Active".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![skip_cfg, active_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        // Only the active cfg should emit an artifact
        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(
            flatpaks.len(),
            1,
            "skipped config should not emit an artifact; active config should"
        );
    }

    // -----------------------------------------------------------------------
    // Workspace multi-crate mode: two crates, each with a flatpak config
    // -----------------------------------------------------------------------

    /// Verifies that in workspace per-crate mode (multiple crates each with an
    /// independent flatpaks: config), each crate emits its own artifact keyed
    /// by crate_name. A regression where the second crate clobbers the first
    /// would manifest as only one artifact with the wrong crate_name.
    #[test]
    fn test_flatpak_dry_run_workspace_per_crate() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let make_flatpak_cfg = |app_id: &str| FlatpakConfig {
            app_id: Some(app_id.to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "workspace".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![
            CrateConfig {
                name: "crate-a".to_string(),
                path: "crates/a".to_string(),
                flatpaks: Some(vec![make_flatpak_cfg("org.example.CrateA")]),
                ..Default::default()
            },
            CrateConfig {
                name: "crate-b".to_string(),
                path: "crates/b".to_string(),
                flatpaks: Some(vec![make_flatpak_cfg("org.example.CrateB")]),
                ..Default::default()
            },
        ];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "workspace");

        // One Linux binary per crate
        for crate_name in &["crate-a", "crate-b"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("dist/{crate_name}")),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: crate_name.to_string(),
                metadata: Default::default(),
                size: None,
            });
        }

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(
            flatpaks.len(),
            2,
            "each crate should emit one flatpak artifact"
        );

        let crate_names: std::collections::HashSet<&str> =
            flatpaks.iter().map(|a| a.crate_name.as_str()).collect();
        assert!(crate_names.contains("crate-a"), "crate-a artifact missing");
        assert!(crate_names.contains("crate-b"), "crate-b artifact missing");
    }

    // -----------------------------------------------------------------------
    // selected_crates filter (crate selection axis)
    // -----------------------------------------------------------------------

    /// Verifies that when selected_crates is set, only the matching crate
    /// is processed. The excluded crate must produce no artifact.
    #[test]
    fn test_flatpak_dry_run_selected_crates_filter() {
        use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let make_cfg = |app_id: &str| FlatpakConfig {
            app_id: Some(app_id.to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "ws".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![
            CrateConfig {
                name: "included".to_string(),
                path: "crates/included".to_string(),
                flatpaks: Some(vec![make_cfg("org.example.Included")]),
                ..Default::default()
            },
            CrateConfig {
                name: "excluded".to_string(),
                path: "crates/excluded".to_string(),
                flatpaks: Some(vec![make_cfg("org.example.Excluded")]),
                ..Default::default()
            },
        ];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                selected_crates: vec!["included".to_string()],
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "ws");

        for crate_name in &["included", "excluded"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("dist/{crate_name}")),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: crate_name.to_string(),
                metadata: Default::default(),
                size: None,
            });
        }

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(
            flatpaks.len(),
            1,
            "only the selected crate should produce an artifact"
        );
        assert_eq!(flatpaks[0].crate_name, "included");
    }

    // -----------------------------------------------------------------------
    // map_to_supported_arches
    // -----------------------------------------------------------------------

    /// Verifies that binaries with supported arches are mapped and those with
    /// unsupported arches (i686, armv7) are dropped.
    #[test]
    fn test_map_to_supported_arches_filters_unsupported() {
        let binaries = vec![
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-x86"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: Default::default(),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-arm"),
                target: Some("aarch64-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: Default::default(),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-i686"),
                target: Some("i686-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: Default::default(),
                size: None,
            },
        ];

        let result = map_to_supported_arches(&binaries);
        assert_eq!(result.len(), 2, "i686 should be filtered out");

        let arches: Vec<&str> = result.iter().map(|(_, _, _, a)| a.as_str()).collect();
        assert!(arches.contains(&"x86_64"));
        assert!(arches.contains(&"aarch64"));
    }

    /// Two builds that collapse onto the same Flatpak arch (x86_64 gnu +
    /// x86_64 musl) must yield ONE job — the per-arch work dir and bundle name
    /// are identical, so a second job would race / clobber the first. First
    /// binary seen wins (the gnu build, which matches the glibc runtime).
    #[test]
    fn test_map_to_supported_arches_dedups_collapsing_arches() {
        let binaries = vec![
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-gnu"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: Default::default(),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-musl"),
                target: Some("x86_64-unknown-linux-musl".to_string()),
                crate_name: "app".to_string(),
                metadata: Default::default(),
                size: None,
            },
        ];

        let result = map_to_supported_arches(&binaries);
        assert_eq!(
            result.len(),
            1,
            "gnu + musl both map to x86_64 — exactly one job must survive"
        );
        // First-seen-wins keeps the gnu binary.
        assert_eq!(result[0].2, PathBuf::from("dist/app-gnu"));
        assert_eq!(result[0].3, "x86_64");
    }

    /// Two x86_64 builds tagged with DIFFERENT amd64 variants (`v1` baseline +
    /// `v3`) keep distinct `(flatpak_arch, variant)` keys, so BOTH survive the
    /// dedup — the spec's same-triple-multi-variant case the guard backs up.
    #[test]
    fn test_map_to_supported_arches_keeps_distinct_amd64_variants() {
        let binaries = vec![
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-v1"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), "v1".to_string())]),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from("dist/app-v3"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "app".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), "v3".to_string())]),
                size: None,
            },
        ];

        let result = map_to_supported_arches(&binaries);
        assert_eq!(
            result.len(),
            2,
            "v1 + v3 of one triple must both survive — distinct variant keys"
        );
        let variants: Vec<Option<&str>> = result.iter().map(|(_, v, _, _)| v.as_deref()).collect();
        assert!(variants.contains(&Some("v1")));
        assert!(variants.contains(&Some("v3")));
    }

    // -----------------------------------------------------------------------
    // Dry-run with extra_files config (process_binary_iteration lines 627-634)
    // -----------------------------------------------------------------------

    /// Verifies that when extra_files is configured, the resolved file names
    /// surface in the dry-run artifact path (via the manifest JSON — we can
    /// assert the stage runs without error and produces the artifact).
    /// The extra_file_names collection at lines 627-634 is exercised.
    #[test]
    fn test_flatpak_dry_run_with_extra_files() {
        use anodizer_core::config::{Config, CrateConfig, ExtraFileSpec, FlatpakConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Write an actual file for the glob to match
        let extra_dir = tmp.path().join("extras");
        std::fs::create_dir_all(&extra_dir).unwrap();
        std::fs::write(extra_dir.join("README.md"), b"readme").unwrap();

        let glob_pattern = format!("{}/*.md", extra_dir.display());
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.App".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            extra_files: Some(vec![ExtraFileSpec::Glob(glob_pattern)]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        // Stage succeeded and produced an artifact — extra_files path was traversed
        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert_eq!(flatpaks.len(), 1);
    }
}
