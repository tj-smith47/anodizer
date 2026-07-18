use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::Artifact;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use super::*;

pub struct FlatpakStage;

/// Output of [`process_binary_iteration`]: either a finished dry-run artifact
/// or a staged live job waiting on the parallel phase.
pub(crate) enum BinaryOutcome {
    DryRun(Artifact),
    Job(FlatpakJob),
}

/// Stage one `(target, binary, flatpak_arch)` triple for a given flatpak
/// config: render templates, build the manifest, and either prepare the
/// Resolved per-cfg identity fields (the four mandatory PKGBUILD-equivalent
/// strings) passed to the per-binary iteration. Bundled to keep the helper's
/// signature under clippy's 7-arg threshold.
pub(crate) struct FlatpakIdentity<'a> {
    app_id: &'a str,
    runtime: &'a str,
    runtime_version: &'a str,
    sdk: &'a str,
}

/// Mutable accumulators threaded through the per-crate / per-cfg loop. The
/// caller seeds empty vectors before the loop and inspects them after; the
/// helpers append to whichever vector matches the per-binary outcome.
pub(crate) struct FlatpakAccumulators<'a> {
    new_artifacts: &'a mut Vec<Artifact>,
    jobs: &'a mut Vec<FlatpakJob>,
    archives_to_remove: &'a mut Vec<PathBuf>,
}

/// dry-run artifact or the live `FlatpakJob`. The caller is responsible for
/// collecting `archives_to_remove` entries from `flatpak_cfg.replace`; this
/// helper performs the lookup against `ctx.artifacts` in both branches via
/// the returned [`BinaryOutcome`] plus the supplied `replace_sink` callback.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_binary_iteration(
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
pub(crate) fn process_flatpak_cfg(
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
                &log,
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
    let configured = ctx
        .config
        .crate_universe()
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
