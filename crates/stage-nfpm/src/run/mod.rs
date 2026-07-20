//! `NfpmStage` — `Stage` implementation that drives `nfpm pkg` per crate / format.
//!
//! The serial phase (`&mut ctx`) renders all templates and writes the YAML into
//! `_tmp_dir`; the parallel phase runs `nfpm pkg --packager <format>`.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::NfpmScripts;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::command::{is_arch_supported_for_format, nfpm_command, validate_format};
use crate::filename;
use crate::generate::{NfpmLibraryPaths, generate_nfpm_yaml_with_env};

mod eligibility;
mod job;
mod render;

#[cfg(test)]
mod tests;

pub(crate) use eligibility::*;
pub(crate) use job::*;
pub use job::{NfpmRenderedConfig, nfpm_yaml_configs_for_crate};
pub(crate) use render::*;

pub struct NfpmStage;

/// Render an `Option<String>` field in place against `vars`.
///
/// `None` is a no-op. Saves ~3 lines per field at the ~15 call sites where
/// nfpm field-by-field templating used to expand the same
/// `if let Some(ref s) = X { X = Some(render(s)?); }` shape inline.
fn render_in_place(
    field: &mut Option<String>,
    vars: &anodizer_core::template::TemplateVars,
) -> Result<()> {
    if let Some(s) = field.as_deref() {
        *field = Some(anodizer_core::template::render(s, vars)?);
    }
    Ok(())
}

/// A fully-staged nfpm job: config YAML written, filename decided,
/// subprocess args composed. Step 1 (serial, `&mut ctx`) renders all
/// templates and writes the YAML into `_tmp_dir`; Step 2 (parallel)
/// runs `nfpm pkg --packager <format>`. `_tmp_dir` keeps the config
/// file alive until the worker thread finishes.
pub(crate) struct NfpmJob {
    _tmp_dir: tempfile::TempDir,
    pkg_path: std::path::PathBuf,
    format: String,
    cmd_args: Vec<String>,
    /// Pre-parsed mtime for reproducible-build mtime stamping, or None
    /// when the config leaves `mtime` unset.
    mtime: Option<std::time::SystemTime>,
    mtime_repr: Option<String>,
    /// Extra environment for the nfpm subprocess. nfpm reads the msix signing
    /// passphrase from ITS OWN env (`NFPM_MSIX_PASSPHRASE`) instead of a YAML
    /// field, so anodizer resolves it from the ctx env map and forwards it here.
    extra_env: Vec<(String, String)>,
    target: Option<String>,
    crate_name: String,
    pkg_metadata: std::collections::HashMap<String, String>,
}

impl Stage for NfpmStage {
    fn name(&self) -> &str {
        "nfpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("nfpm");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have nfpm config
        let crates: Vec<_> = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nfpms.is_some())
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

        // when the global skip_sign is active, zero out
        // all nFPM signature configuration in the generated YAML.
        let skip_sign = ctx.should_skip("sign");

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut jobs: Vec<NfpmJob> = Vec::new();

        validate_unique_config_ids(&crates)?;

        for krate in &crates {
            collect_nfpm_jobs_for_crate(
                ctx,
                &log,
                krate,
                &dist,
                &version,
                skip_sign,
                dry_run,
                &mut new_artifacts,
                &mut jobs,
            )?;
        }

        clear_nfpm_template_vars(ctx);

        if !jobs.is_empty() {
            let results = execute_nfpm_jobs(&jobs, parallelism, log.verbosity())?;
            new_artifacts.extend(results);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Collect nfpm build jobs for one crate: iterates configs, platform groups,
/// and formats, staging YAML and populating `new_artifacts` (dry-run) or
/// `jobs` (live run).
#[allow(clippy::too_many_arguments)]
fn collect_nfpm_jobs_for_crate(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    krate: &anodizer_core::config::CrateConfig,
    dist: &std::path::Path,
    version: &str,
    skip_sign: bool,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    jobs: &mut Vec<NfpmJob>,
) -> Result<()> {
    let Some(nfpm_configs) = krate.nfpms.as_ref() else {
        return Ok(());
    };

    let linux_binaries = nfpm_eligible_artifacts(ctx, &krate.name);

    // One guard per crate spans every `nfpms:` config of that crate: two configs
    // with the same format + arch and the default (or identical) filename render
    // the same package path — error loudly across configs instead of letting the
    // second silently clobber the first.
    let mut name_guard = anodizer_core::arch_path_guard::ArchPathGuard::new();

    for nfpm_cfg in nfpm_configs {
        let nfpm_id_for_log = nfpm_cfg.id.as_deref().unwrap_or("default").to_string();

        if should_skip_nfpm_config(ctx, nfpm_cfg, &nfpm_id_for_log, log)? {
            continue;
        }

        let is_meta = nfpm_cfg.meta == Some(true);

        let Some(platform_groups) =
            build_platform_groups(nfpm_cfg, krate, &linux_binaries, is_meta, log)
        else {
            continue;
        };

        for (target, amd64_variant, binary_paths, lib_paths) in &platform_groups {
            let (base_os, base_arch) = target
                .as_deref()
                .map(anodizer_core::target::map_target)
                .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

            for format in &nfpm_cfg.formats {
                process_nfpm_format(
                    ctx,
                    log,
                    nfpm_cfg,
                    &krate.name,
                    &linux_binaries,
                    target,
                    amd64_variant.as_deref(),
                    binary_paths,
                    lib_paths,
                    &base_os,
                    &base_arch,
                    format,
                    dist,
                    version,
                    skip_sign,
                    dry_run,
                    new_artifacts,
                    jobs,
                    &mut name_guard,
                )?;
            }
        }
    }

    Ok(())
}

/// Render, validate, and stage one nfpm format for one platform group.
///
/// Adds a dry-run artifact to `new_artifacts` or a live `NfpmJob` to `jobs`.
#[allow(clippy::too_many_arguments)]
fn process_nfpm_format(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    linux_binaries: &[Artifact],
    target: &Option<String>,
    amd64_variant: Option<&str>,
    binary_paths: &[String],
    lib_paths: &NfpmLibraryPaths,
    base_os: &str,
    base_arch: &str,
    format: &str,
    dist: &std::path::Path,
    version: &str,
    skip_sign: bool,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    jobs: &mut Vec<NfpmJob>,
    name_guard: &mut anodizer_core::arch_path_guard::ArchPathGuard,
) -> Result<()> {
    validate_format(format).with_context(|| format!("nfpm config for crate {}", crate_name))?;

    // msix is the only Windows format, and it only packages Windows binaries
    // (mirrors GoReleaser's windows↔msix XOR gate). Silent-verbose skip, not
    // a strict guard: a multi-format config legitimately routes each target
    // to its matching subset of formats.
    if !format_matches_platform(base_os, format) {
        log.verbose(&format!(
            "skipped nfpm format '{}' for {}/{} — not supported",
            format, base_os, base_arch
        ));
        return Ok(());
    }

    let Some((os, arch)) = resolve_format_os_arch(base_os, base_arch, format, log) else {
        return Ok(());
    };

    if let Some(triple) = target.as_deref()
        && !is_arch_supported_for_format(triple, format)
    {
        ctx.strict_guard(
            log,
            &format!(
                "skipped nfpm format '{}' for target '{}' — architecture not supported",
                format, triple
            ),
        )?;
        return Ok(());
    }

    // Require the maintainer only once we know a deb/apk WILL be built for
    // this (format × target): the two early returns above mean no package is
    // produced for an unsupported/skipped arch, so a missing maintainer must
    // not false-fail a config whose only target is skipped. A deb/apk that
    // genuinely builds still hard-fails when no maintainer can be resolved.
    require_deb_apk_maintainer(&ctx.config, nfpm_cfg, crate_name, format)?;
    require_msix_essentials(nfpm_cfg, crate_name, format)?;

    let pkg_name_owned = resolve_pkg_name(nfpm_cfg, &ctx.config.project_name, crate_name);
    let pkg_name: &str = pkg_name_owned.as_str();
    let ext = format_extension(format);

    // Seed `Amd64` BEFORE rendering so a config field referencing `{{ .Amd64 }}`
    // (description/maintainer/conflicts/…) AND the `file_name_template` both see
    // this group's micro-arch variant. The conventional default filename
    // deliberately omits the variant (deb/rpm/apk require a bare `amd64` arch
    // field); the guard below is what stops two variants from colliding under
    // that default. `None` on an amd64 binary seeds the unified `v1` baseline
    // (same value every seeding policy gives an untagged x86_64 binary).
    anodizer_core::archive_name::seed_amd64_variant_var(
        ctx.template_vars_mut(),
        base_arch,
        amd64_variant,
    );

    let yaml_content = render_and_generate_nfpm_yaml(
        ctx,
        nfpm_cfg,
        crate_name,
        linux_binaries,
        target.as_deref(),
        binary_paths,
        lib_paths,
        &os,
        &arch,
        format,
        pkg_name,
        dist,
        version,
        skip_sign,
        dry_run,
    )?;

    // msix is a Windows package; it lands next to the MSI/NSIS outputs.
    let output_dir = if format == "msix" {
        dist.join("windows")
    } else {
        dist.join("linux")
    };
    if !dry_run {
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("create nfpm output dir: {}", output_dir.display()))?;
    }

    set_nfpm_per_pkg_template_vars(
        ctx,
        nfpm_cfg,
        &os,
        &arch,
        target.as_deref(),
        format,
        pkg_name,
        ext,
        version,
    );

    let pkg_filename = compute_pkg_filename(
        ctx,
        nfpm_cfg,
        crate_name,
        target.as_deref(),
        pkg_name,
        version,
        &os,
        &arch,
        ext,
    )?;
    let pkg_path = output_dir.join(&pkg_filename);

    // A user `file_name_template` is echoed verbatim into a collision error so
    // the user sees the template at fault; the conventional default has no real
    // template, so its dedicated path names "the conventional default filename"
    // and advises `{{ .Amd64 }}` (the default already carries `{{ .Arch }}`).
    match nfpm_cfg.file_name_template.as_deref() {
        Some(name_template) => name_guard.check(
            &pkg_path,
            "nfpms",
            "package",
            name_template,
            &pkg_filename,
            crate_name,
        )?,
        None => name_guard.check_conventional(
            &pkg_path,
            "nfpms",
            "package",
            &pkg_filename,
            crate_name,
        )?,
    }

    let mut pkg_metadata = HashMap::from([("format".to_string(), format.to_string())]);
    if let Some(ref id) = nfpm_cfg.id {
        pkg_metadata.insert("id".to_string(), id.clone());
    }
    // Record the micro-arch variant so the offline schema validator can pair a
    // built package with the exact per-variant config it was rendered from: two
    // amd64 variants of one triple share (format, target), and a `{{ .Amd64 }}`
    // in a control field makes their YAML differ.
    if let Some(variant) = amd64_variant {
        pkg_metadata.insert("amd64_variant".to_string(), variant.to_string());
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) would run: nfpm pkg --packager {format} for crate {} target {:?}",
            crate_name, target
        ));
        new_artifacts.push(Artifact {
            kind: artifact_kind_for_format(format),
            name: String::new(),
            path: pkg_path,
            target: target.clone(),
            crate_name: crate_name.to_string(),
            metadata: pkg_metadata,
            size: None,
        });
        return Ok(());
    }

    jobs.push(build_nfpm_job(
        ctx,
        nfpm_cfg,
        &yaml_content,
        &pkg_path,
        format,
        target.as_deref(),
        crate_name,
        pkg_metadata,
        log,
    )?);

    Ok(())
}
