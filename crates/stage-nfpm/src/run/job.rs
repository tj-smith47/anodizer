use super::*;

/// Fill `deb.arch_variant` from the per-target artifact's `amd64_variant`
/// (GOAMD64 microarch) metadata when the user has not set it explicitly, so an
/// amd64 deb is tagged with the microarchitecture it was built for.
pub(crate) fn fill_deb_arch_variant(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    linux_binaries: &[Artifact],
    target: Option<&str>,
) {
    if let Some(ref mut deb) = rendered_cfg.deb
        && deb.arch_variant.is_none()
        && let Some(t) = target
    {
        let variant = linux_binaries
            .iter()
            .find(|b| b.target.as_deref() == Some(t))
            .and_then(|b| b.metadata.get("amd64_variant").cloned());
        deb.arch_variant = variant;
    }
}

/// Resolve the package name following this precedence:
/// explicit `package_name`, then project-level `project_name`, then the
/// crate name as last-resort fallback.
pub(crate) fn resolve_pkg_name(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    project_name: &str,
    crate_name: &str,
) -> String {
    if let Some(n) = nfpm_cfg.package_name.as_deref() {
        n.to_string()
    } else if !project_name.is_empty() {
        project_name.to_string()
    } else {
        crate_name.to_string()
    }
}

/// Populate the per-target template variables (`Os`, `Arch`, `Target`,
/// `Libc`) shared by every per-target field that renders for one
/// (config × target) iteration.
///
/// Called before `render_nfpm_config_fields` so `conflicts`/`provides`/
/// `replaces` resolve against THIS target, then again (transitively, via
/// `set_nfpm_per_pkg_template_vars`) before the filename template renders.
/// `Libc` is `musl`/`gnu` for the respective triples, empty when the target
/// has no libc concept.
pub(crate) fn set_nfpm_per_target_template_vars(
    ctx: &mut Context,
    os: &str,
    arch: &str,
    target: Option<&str>,
) {
    ctx.template_vars_mut().set("Os", os);
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    ctx.template_vars_mut().set(
        "Libc",
        target
            .map(anodizer_core::target::libc_from_target)
            .unwrap_or(""),
    );
}

/// Populate per-package template variables (`Os`, `Arch`, `Target`, `Libc`,
/// `Format`, `PackageName`, `ConventionalExtension`,
/// `ConventionalFileName`, `Release`, `Epoch`) before rendering the
/// user's `file_name_template`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_nfpm_per_pkg_template_vars(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    os: &str,
    arch: &str,
    target: Option<&str>,
    format: &str,
    pkg_name: &str,
    ext: &str,
    version: &str,
) {
    set_nfpm_per_target_template_vars(ctx, os, arch, target);
    ctx.template_vars_mut().set("Format", format);
    ctx.template_vars_mut().set("PackageName", pkg_name);
    ctx.template_vars_mut().set("ConventionalExtension", ext);
    let mut fn_info = filename::FileNameInfo::from_config(nfpm_cfg, pkg_name, version, arch);
    // `msix.arch` overrides the derived MSIX arch verbatim (nfpm's
    // ensureValidArch precedence).
    if format == "msix" {
        fn_info.arch_override = nfpm_cfg.msix.as_ref().and_then(|m| m.arch.as_deref());
    }
    let conventional = filename::conventional_filename(format, &fn_info)
        .unwrap_or_else(|| format!("{pkg_name}_{version}_{os}_{arch}{ext}"));
    ctx.template_vars_mut()
        .set("ConventionalFileName", &conventional);
    ctx.template_vars_mut()
        .set("Release", nfpm_cfg.release.as_deref().unwrap_or(""));
    ctx.template_vars_mut()
        .set("Epoch", nfpm_cfg.epoch.as_deref().unwrap_or(""));
}

/// Render `file_name_template` to a concrete filename, appending the
/// format-specific extension when the rendered template didn't already
/// end with it. Falls back to the hand-rolled `<name>_<ver>_<os>_<arch>`
/// pattern when no template is configured.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_pkg_filename(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    target: Option<&str>,
    pkg_name: &str,
    version: &str,
    os: &str,
    arch: &str,
    ext: &str,
) -> Result<String> {
    let pkg_filename = if let Some(tmpl) = &nfpm_cfg.file_name_template {
        let rendered = ctx.render_template(tmpl).with_context(|| {
            format!(
                "nfpm: render file_name_template for crate {} target {:?}",
                crate_name, target
            )
        })?;
        if !ext.is_empty() && rendered.ends_with(ext) {
            rendered
        } else {
            format!("{rendered}{ext}")
        }
    } else {
        format!("{pkg_name}_{version}_{os}_{arch}{ext}")
    };
    Ok(pkg_filename)
}

/// Build a fully-prepared `NfpmJob`: write the generated YAML into a
/// per-job tempdir, compose the `nfpm pkg --packager <format>` args, and
/// pre-parse the user's `mtime` so the parallel worker doesn't touch
/// `ctx`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_nfpm_job(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    yaml_content: &str,
    pkg_path: &std::path::Path,
    format: &str,
    target: Option<&str>,
    crate_name: &str,
    pkg_metadata: HashMap<String, String>,
    log: &anodizer_core::log::StageLogger,
) -> Result<NfpmJob> {
    let tmp_dir = tempfile::tempdir().context("create temp dir for nfpm config")?;
    let config_path = tmp_dir.path().join("nfpm.yaml");
    fs::write(&config_path, yaml_content)
        .with_context(|| format!("write nfpm config to {}", config_path.display()))?;

    let cmd_args = nfpm_command(
        &config_path.to_string_lossy(),
        format,
        &pkg_path.to_string_lossy(),
    );

    let (mtime, mtime_repr) = if let Some(ref raw_mtime) = nfpm_cfg.mtime {
        let rendered_mtime = ctx
            .render_template(raw_mtime)
            .with_context(|| format!("nfpm: render mtime template '{raw_mtime}'"))?;
        match anodizer_core::util::parse_mod_timestamp(&rendered_mtime) {
            Ok(mt) => (Some(mt), Some(rendered_mtime)),
            Err(e) => {
                log.warn(&format!("invalid nfpm mtime '{rendered_mtime}': {e}"));
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // The msix passphrase cannot be embedded in the YAML (nfpm declares the
    // field `yaml:"-"` and reads NFPM_MSIX_PASSPHRASE from its process env),
    // so resolve it through the same NFPM_{ID}_{format}_PASSPHRASE →
    // NFPM_{ID}_PASSPHRASE → NFPM_PASSPHRASE ladder the other formats use and
    // forward it via the subprocess env. Only when a signature is configured
    // and signing is not skipped.
    let mut extra_env = Vec::new();
    if format == "msix"
        && !ctx.should_skip("sign")
        && nfpm_cfg
            .msix
            .as_ref()
            .is_some_and(|m| m.signature.is_some())
        && let Some(passphrase) = crate::builders::resolve_passphrase_from_env(
            ctx.template_vars().all_env(),
            nfpm_cfg.id.as_deref().unwrap_or("default"),
            // uppercase to match the documented NFPM_<ID>_MSIX_PASSPHRASE name
            Some("MSIX"),
        )
    {
        extra_env.push(("NFPM_MSIX_PASSPHRASE".to_string(), passphrase));
    }

    Ok(NfpmJob {
        _tmp_dir: tmp_dir,
        pkg_path: pkg_path.to_path_buf(),
        format: format.to_string(),
        cmd_args,
        mtime,
        mtime_repr,
        extra_env,
        target: target.map(str::to_string),
        crate_name: crate_name.to_string(),
        pkg_metadata,
    })
}

/// Clear the per-target + per-packaging template variables once all jobs
/// have been prepared, so leaked state doesn't reach downstream stages
/// like `announce` or `publish`.
pub(crate) fn clear_nfpm_template_vars(ctx: &mut Context) {
    anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());
    for extra in [
        "Format",
        "PackageName",
        "ConventionalExtension",
        "ConventionalFileName",
        "Release",
        "Epoch",
    ] {
        ctx.template_vars_mut().set(extra, "");
    }
}

/// Run all prepared nfpm jobs in parallel with bounded concurrency. Each
/// worker invokes `nfpm pkg`, applies the reproducible-build mtime, and
/// returns a populated `Artifact` for serial registration by the caller.
pub(crate) fn execute_nfpm_jobs(
    jobs: &[NfpmJob],
    parallelism: usize,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Vec<Artifact>> {
    let log = anodizer_core::log::StageLogger::new("nfpm", verbosity);
    let run_job = |job: &NfpmJob| -> Result<Artifact> {
        let thread_log = anodizer_core::log::StageLogger::new("nfpm", verbosity);

        thread_log.verbose(&format!("running {}", job.cmd_args.join(" ")));

        let mut cmd = Command::new(&job.cmd_args[0]);
        cmd.args(&job.cmd_args[1..]);
        for (k, v) in &job.extra_env {
            cmd.env(k, v);
        }
        let output = cmd.output().with_context(|| {
            format!(
                "execute nfpm for format {} (crate {} target {:?})",
                job.format, job.crate_name, job.target
            )
        })?;
        // Older nfpm binaries report an unregistered packager for msix; point
        // the user at the version floor. Only on that signature — attaching it
        // to every msix failure (config validation, IO) misdirects users whose
        // nfpm is already new enough. nfpm prints errors to stdout, which the
        // error chain doesn't embed, so probe the captured output directly.
        let unregistered_packager = job.format == "msix"
            && [&output.stdout, &output.stderr]
                .iter()
                .any(|s| String::from_utf8_lossy(s).contains("no packager registered"));
        let checked = thread_log.check_output(output, "nfpm");
        match checked {
            Err(e) if unregistered_packager => {
                return Err(e.context("the 'msix' packager requires nfpm >= 2.46.0"));
            }
            other => other?,
        };

        let pkg_name = job
            .pkg_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.pkg_path.display().to_string());
        thread_log.status(&format!("packed {pkg_name}"));

        if let Some(mt) = job.mtime {
            // A failure here would leave the package carrying a wall-clock
            // mtime, breaking byte-reproducibility. Determinism is not
            // best-effort: fail hard rather than ship a non-reproducible pkg.
            anodizer_core::util::set_file_mtime(&job.pkg_path, mt).with_context(|| {
                format!(
                    "nfpm: apply reproducible mtime to {}",
                    job.pkg_path.display()
                )
            })?;
            if let Some(ref repr) = job.mtime_repr {
                thread_log.verbose(&format!(
                    "applied mtime={repr} to {}",
                    job.pkg_path.display()
                ));
            }
        }

        Ok(Artifact {
            kind: artifact_kind_for_format(&job.format),
            name: String::new(),
            path: job.pkg_path.clone(),
            target: job.target.clone(),
            crate_name: job.crate_name.clone(),
            metadata: job.pkg_metadata.clone(),
            size: None,
        })
    };

    anodizer_core::parallel::run_parallel_chunks(jobs, parallelism, "nfpm", &log, run_job)
}

/// One nfpm YAML config a build would feed to `nfpm pkg` for a single
/// (config × target × format) combination, rendered offline for schema
/// validation.
pub struct NfpmRenderedConfig {
    /// nfpm packager format this config targets (`deb`, `rpm`, `apk`, …).
    pub format: String,
    /// Target triple the config was rendered for, or empty when the crate
    /// built a host binary with no triple.
    pub target: String,
    /// Resolved package architecture stamped into the config (`amd64`,
    /// `arm64`, …) — the value nfpm would otherwise default to `amd64`.
    pub arch: String,
    /// The amd64 micro-arch variant this config was rendered for (`None`/`v1`
    /// → baseline). Two amd64 variants of one triple share `(format, target)`,
    /// so a consumer pairing a built package with its source config must also
    /// key on this to avoid validating a `v3` package against the `v1` config.
    pub amd64_variant: Option<String>,
    /// The generated nfpm YAML, ready to parse and validate against nfpm's
    /// own config schema.
    pub yaml: String,
}

/// Render every nfpm config a build would feed to `nfpm pkg` for one crate,
/// mirroring the build's per-(config × target × format) `run` walk — without
/// writing files or spawning `nfpm`.
///
/// Returns `Ok(vec![])` (nothing to validate) when the crate carries no nfpm
/// config, when a config's `if:` gate evaluates falsy, when a config sets no
/// output formats, when the `ids` filter admits no eligible binary, or when no
/// packaging-eligible artifact was built for the crate in this snapshot shard
/// (the same shard-tolerance cases the build's skip guards hit). Otherwise it
/// walks the SAME shared helpers the build loop uses
/// (`nfpm_eligible_artifacts`, `nfpm_config_if_proceeds`,
/// `build_platform_groups`, `resolve_format_os_arch`,
/// `is_arch_supported_for_format`, `render_nfpm_config_fields`) and returns one
/// rendered config per combination, each stamped with the run's resolved
/// version and target architecture.
///
/// The on-disk `templated_contents` / `templated_scripts` / lintian-override
/// passes the build runs are intentionally not replayed here: they only append
/// `contents:` entries sourced from external files and never change the
/// schema-relevant shape of the config anodizer controls. A genuine render
/// error (a malformed template in a config field) propagates as `Err` — it is
/// never swallowed as a shard skip.
pub fn nfpm_yaml_configs_for_crate(
    ctx: &Context,
    crate_name: &str,
) -> Result<Vec<NfpmRenderedConfig>> {
    let log = ctx.logger("nfpm");
    let Some(krate) = ctx.config.find_crate(crate_name) else {
        return Ok(Vec::new());
    };
    let Some(nfpm_configs) = krate.nfpms.as_ref() else {
        return Ok(Vec::new());
    };

    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string());
    let skip_sign = ctx.should_skip("sign");

    let linux_binaries = nfpm_eligible_artifacts(ctx, crate_name);

    let mut rendered = Vec::new();
    for nfpm_cfg in nfpm_configs {
        let nfpm_id_for_log = nfpm_cfg.id.as_deref().unwrap_or("default").to_string();

        // A falsy `if:` or an empty `formats:` suppresses the config in the
        // build, so it renders no YAML here either.
        if !nfpm_config_if_proceeds(ctx, nfpm_cfg, &nfpm_id_for_log)? {
            continue;
        }
        if nfpm_cfg.formats.is_empty() {
            continue;
        }

        let is_meta = nfpm_cfg.meta == Some(true);
        let Some(platform_groups) =
            build_platform_groups(nfpm_cfg, krate, &linux_binaries, is_meta, &log)
        else {
            // `ids:` filter matched no binary — the build skips this config.
            continue;
        };

        // Same name resolution the live build threads to the YAML's `name:`,
        // so the offline-validated config is byte-identical to the shipped one.
        let pkg_name = resolve_pkg_name(nfpm_cfg, &ctx.config.project_name, crate_name);

        for (target, amd64_variant, binary_paths, lib_paths) in &platform_groups {
            let (base_os, base_arch) = target
                .as_deref()
                .map(anodizer_core::target::map_target)
                .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

            for format in &nfpm_cfg.formats {
                validate_format(format)
                    .with_context(|| format!("nfpm config for crate {crate_name}"))?;

                if !format_matches_platform(&base_os, format) {
                    continue;
                }

                let Some((os, arch)) = resolve_format_os_arch(&base_os, &base_arch, format, &log)
                else {
                    continue;
                };

                if let Some(triple) = target.as_deref()
                    && !is_arch_supported_for_format(triple, format)
                {
                    continue;
                }

                let render_target = crate::generate::NfpmRenderTarget {
                    pkg_name: &pkg_name,
                    os: &os,
                    arch: &arch,
                    target: target.as_deref(),
                    format: Some(format),
                    version: &version,
                    skip_sign,
                };
                let yaml = render_offline_nfpm_yaml(
                    ctx,
                    nfpm_cfg,
                    crate_name,
                    &render_target,
                    amd64_variant.as_deref(),
                    &linux_binaries,
                    binary_paths,
                    lib_paths,
                )?;

                rendered.push(NfpmRenderedConfig {
                    format: format.clone(),
                    target: target.clone().unwrap_or_default(),
                    arch,
                    amd64_variant: amd64_variant.clone(),
                    yaml,
                });
            }
        }
    }

    Ok(rendered)
}

/// Render one (config × target × format) nfpm YAML against a per-target
/// clone of the template vars, without mutating `ctx`. The clone carries the
/// same `Os`/`Arch`/`Target`/`Libc` the build sets per target, so relationship
/// lists (`conflicts`/`provides`/…) resolve their `{{ .Libc }}` etc. exactly
/// as the live build does — the offline render emits what the build feeds nfpm.
///
/// `linux_binaries` is threaded so the deb `arch_variant` the live build
/// auto-derives from a target's `amd64_variant` metadata
/// (`fill_deb_arch_variant`) is present in the validated YAML too, keeping the
/// validated config byte-identical to the shipped one.
///
/// `amd64_variant` seeds the `Amd64` template var on the cloned vars, mirroring
/// the live build, so a config field referencing `{{ .Amd64 }}` renders the
/// same per-variant value offline as it ships.
#[allow(clippy::too_many_arguments)]
fn render_offline_nfpm_yaml(
    ctx: &Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    render_target: &crate::generate::NfpmRenderTarget<'_>,
    amd64_variant: Option<&str>,
    linux_binaries: &[Artifact],
    binary_paths: &[String],
    lib_paths: &NfpmLibraryPaths,
) -> Result<String> {
    let mut vars = ctx.template_vars().clone();
    vars.set("Os", render_target.os);
    vars.set("Arch", render_target.arch);
    vars.set("Target", render_target.target.unwrap_or(""));
    vars.set(
        "Libc",
        render_target
            .target
            .map(anodizer_core::target::libc_from_target)
            .unwrap_or(""),
    );
    anodizer_core::archive_name::seed_amd64_variant_var(
        &mut vars,
        render_target.arch,
        amd64_variant,
    );

    let mut rendered_cfg = render_nfpm_config_fields(nfpm_cfg, &ctx.config, &vars, crate_name)?;
    default_nfpm_mtime_to_sde(&mut rendered_cfg, ctx.env_source());
    fill_deb_arch_variant(&mut rendered_cfg, linux_binaries, render_target.target);

    generate_nfpm_yaml_with_env(
        &rendered_cfg,
        render_target,
        binary_paths,
        lib_paths,
        ctx.template_vars().all_env(),
    )
}
