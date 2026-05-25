use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

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

/// Default output filename template for Flatpak bundles.
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.flatpak";

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

/// Map filtered binaries onto `(target, path, flatpak_arch)` tuples,
/// dropping any architecture Flatpak doesn't support.
fn map_to_supported_arches(binaries: &[Artifact]) -> Vec<(Option<String>, PathBuf, String)> {
    binaries
        .iter()
        .filter_map(|b| {
            let (_, arch) = os_arch_from_target(b.target.as_deref());
            arch_to_flatpak(&arch)
                .map(|flatpak_arch| (b.target.clone(), b.path.clone(), flatpak_arch.to_string()))
        })
        .collect()
}

/// Render the bundle output filename via `name_template`, defaulting to
/// `DEFAULT_NAME_TEMPLATE`, and force a `.flatpak` suffix.
fn render_output_filename(
    ctx: &Context,
    flatpak_cfg: &anodizer_core::config::FlatpakConfig,
    crate_name: &str,
    target: &Option<String>,
) -> Result<String> {
    let name_template = flatpak_cfg
        .name_template
        .as_deref()
        .unwrap_or(DEFAULT_NAME_TEMPLATE);
    let rendered = ctx.render_template(name_template).with_context(|| {
        format!(
            "flatpak: render name template for crate {} target {:?}",
            crate_name, target
        )
    })?;
    Ok(if rendered.to_lowercase().ends_with(".flatpak") {
        rendered
    } else {
        format!("{rendered}.flatpak")
    })
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

    thread_log.status(&format!("running: {}", job.builder_args.join(" ")));
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

    thread_log.status(&format!("running: {}", job.bundle_args.join(" ")));
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
    app_id: &str,
    runtime: &str,
    runtime_version: &str,
    sdk: &str,
    version: &str,
    target: &Option<String>,
    binary_path: &std::path::Path,
    flatpak_arch: &str,
    dry_run: bool,
    archives_to_remove: &mut Vec<PathBuf>,
) -> Result<BinaryOutcome> {
    let (os, arch) = os_arch_from_target(target.as_deref());
    ctx.template_vars_mut().set("Os", &os);
    ctx.template_vars_mut().set("Arch", &arch);
    ctx.template_vars_mut()
        .set("Target", target.as_deref().unwrap_or(""));

    let output_name = render_output_filename(ctx, flatpak_cfg, &krate.name, target)?;

    let output_dir = dist.join("flatpak");
    let output_path = output_dir.join(&output_name);
    let work_dir = dist.join("flatpak").join(&krate.name).join(flatpak_arch);

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
        app_id,
        runtime,
        runtime_version,
        sdk,
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
        app_id,
    )?;

    let (builder_args, bundle_args) =
        build_subprocess_args(app_id, version, flatpak_arch, &output_path);

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
    new_artifacts: &mut Vec<Artifact>,
    jobs: &mut Vec<FlatpakJob>,
    archives_to_remove: &mut Vec<PathBuf>,
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

    let mut filtered = linux_binaries.to_vec();
    filter_binaries_by_ids(&mut filtered, flatpak_cfg.ids.as_ref());

    if filtered.is_empty() && linux_binaries.is_empty() {
        log.warn(&format!(
            "no Linux binary artifacts found for crate '{}'; \
             skipping Flatpak generation (expected binaries targeting linux)",
            krate.name
        ));
        return Ok(());
    }
    if filtered.is_empty() {
        log.warn(&format!(
            "ids filter {:?} matched no binaries for crate '{}'; skipping",
            flatpak_cfg.ids, krate.name
        ));
        return Ok(());
    }

    let effective_binaries = map_to_supported_arches(&filtered);
    if effective_binaries.is_empty() {
        log.warn(&format!(
            "no supported architectures (amd64/arm64) found for crate '{}'; skipping Flatpak",
            krate.name
        ));
        return Ok(());
    }

    for (target, binary_path, flatpak_arch) in &effective_binaries {
        let outcome = process_binary_iteration(
            ctx,
            log,
            dist,
            krate,
            flatpak_cfg,
            app_id,
            runtime,
            runtime_version,
            sdk,
            version,
            target,
            binary_path,
            flatpak_arch,
            dry_run,
            archives_to_remove,
        )?;
        match outcome {
            BinaryOutcome::DryRun(artifact) => new_artifacts.push(artifact),
            BinaryOutcome::Job(job) => jobs.push(job),
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

            for flatpak_cfg in flatpak_configs {
                process_flatpak_cfg(
                    ctx,
                    &log,
                    &dist,
                    krate,
                    flatpak_cfg,
                    &linux_binaries,
                    &version,
                    dry_run,
                    &mut new_artifacts,
                    &mut jobs,
                    &mut archives_to_remove,
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
        assert_eq!(
            DEFAULT_NAME_TEMPLATE,
            "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.flatpak"
        );
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
}
