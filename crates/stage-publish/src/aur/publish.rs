use super::*;

// ---------------------------------------------------------------------------
// publish_to_aur
// ---------------------------------------------------------------------------

/// A rendered AUR package: the `PKGBUILD` Bash script and its `.SRCINFO`
/// metadata sidecar, exactly as a live publish would write them, plus the
/// resolved package name they carry.
///
/// Produced by [`render_aur_pkgbuild_and_srcinfo_for_crate`] (binary) and the
/// source-AUR render fns so the offline schema validator checks the
/// byte-identical artifacts the publish path ships.
#[derive(Debug)]
pub(crate) struct AurRendered {
    /// The rendered `PKGBUILD` Bash script body.
    pub(crate) pkgbuild: String,
    /// The rendered `.SRCINFO` metadata body.
    pub(crate) srcinfo: String,
    /// The resolved, post-template package name stamped into both artifacts.
    /// Threaded out so the live write/push path reuses it instead of
    /// re-resolving (and re-warning on) the `aur.name` template a second time.
    pub(crate) package_name: String,
}

/// `Ok(true)` when at least one Linux archive survives the AUR filters for
/// `crate_name` — i.e. [`aur_build_sources`] has a candidate to point a
/// `source_<arch>=` line at. `Ok(false)` when NO artifact matches (genuine
/// absence): a sharded snapshot that built no matching Linux archive, which the
/// validator treats as a skip rather than tripping the publisher's "no linux
/// archives matched" guard.
///
/// This distinguishes ABSENCE from ERROR by propagating the `Err`: the
/// underlying [`util::find_artifacts_by_os_with_variant`] returns `Err` when a
/// MATCHED artifact is missing its sha256 (the same error the live publish path
/// `?`s at [`aur_build_sources`]), and that `Err` flows through here so the
/// caller surfaces a matched-but-broken artifact rather than silently skipping
/// it. Only a clean `Ok(empty)` (true absence) skips.
pub(crate) fn crate_has_aur_linux_archive(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
) -> Result<bool> {
    let ids_filter = aur_cfg.ids.as_deref();
    let amd64_variant = aur_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let matched = util::find_artifacts_by_os_with_variant(
        ctx,
        crate_name,
        "linux",
        ids_filter,
        Some(amd64_variant),
        Some("7"),
    )?;
    Ok(!matched.is_empty())
}

/// Skip-unaware render of a binary-AUR `PKGBUILD` + `.SRCINFO` for
/// `crate_name`. Resolves the field defaults, builds the `source_<arch>=`
/// tuples, assembles [`PkgbuildParams`], and renders both artifacts.
///
/// The skip / `if` / `skip_upload` gate is evaluated by the callers — both the
/// live publish path (via [`aur_check_skip_and_resolve_git_url`]) and
/// [`render_aur_pkgbuild_and_srcinfo_for_crate`] — so each
/// resolved-with-warning value is logged exactly once and the gate is never
/// double-evaluated.
pub(crate) fn render_aur_inner(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<AurRendered> {
    let fields = aur_resolve_fields(ctx, crate_cfg, aur_cfg, crate_name, log)?;
    let sources = aur_build_sources(ctx, aur_cfg, crate_name, &fields.version)?;

    // Compute .install filename: strip trailing "-bin" from the package name.
    let install_base = fields
        .package_name
        .strip_suffix("-bin")
        .unwrap_or(&fields.package_name);
    let install_filename = format!("{}.install", install_base);
    let install_file_ref = if aur_cfg.install.is_some() {
        Some(install_filename.as_str())
    } else {
        None
    };

    // LICENSE/man/completion install lines appended to the default `package()`
    // body (ignored when `aur.package` overrides the whole body). The AUR -bin
    // binary name is the crate name.
    let extra_install_lines = aur_extra_install_lines(crate_cfg, crate_name);

    let pkgbuild_params = PkgbuildParams {
        name: &fields.package_name,
        version: &fields.version,
        pkgrel: fields.pkgrel,
        description: &fields.description,
        url: &fields.url,
        license: &fields.license,
        maintainers: &fields.maintainers,
        contributors: &fields.contributors,
        depends: &fields.depends,
        optdepends: &fields.optdepends,
        conflicts: &fields.conflicts,
        provides: &fields.provides,
        replaces: &fields.replaces,
        backup: &fields.backup,
        sources: &sources,
        binary_name: crate_name,
        install_template: aur_cfg.package.as_deref(),
        extra_install_lines: &extra_install_lines,
        install_file: install_file_ref,
    };
    let pkgbuild = generate_pkgbuild(&pkgbuild_params)?;
    let srcinfo = generate_srcinfo(&pkgbuild_params)?;
    util::guard_no_unrendered(ctx, log, "aur PKGBUILD", &pkgbuild)?;
    util::guard_no_unrendered(ctx, log, "aur .SRCINFO", &srcinfo)?;
    Ok(AurRendered {
        pkgbuild,
        srcinfo,
        package_name: fields.package_name,
    })
}

/// Render the binary-AUR `PKGBUILD` + `.SRCINFO` a live publish would write
/// for `crate_name`, honoring `skip` / `skip_upload` / the `if:` condition.
///
/// Returns `Ok(None)` when the publisher would skip this crate (a truthy
/// `skip` / `skip_upload` or a falsy `if`) — nothing to render or validate.
/// The live publish path and the offline schema validator both produce the
/// artifacts through the same skip-unaware [`render_aur_inner`], so the
/// validated document is byte-for-byte what a release pushes.
///
/// Errors when the crate carries no `aur` block, when no Linux archive matches
/// the configured filters, or when a matched artifact is missing its sha256 (a
/// release always builds at least one valid archive). A sharded snapshot that
/// built no matching archive surfaces as that error; the validator treats it
/// as a skip via [`crate_has_aur_linux_archive`].
pub(crate) fn render_aur_pkgbuild_and_srcinfo_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<AurRendered>> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "aur")?;
    let aur_cfg = publish
        .aur
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no aur config for '{}'", crate_name))?;

    // `skip` (truthy) suppresses the crate entirely.
    if let Some(ref d) = aur_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("aur: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "skipped aur for '{}' — skip evaluates true",
                crate_name
            ));
            return Ok(None);
        }
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        aur_cfg.if_condition.as_deref(),
        &format!("aur publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped aur for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(None);
    }

    if crate::util::should_skip_upload(
        aur_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("aur for '{crate_name}'")),
    )? {
        return Ok(None);
    }

    Ok(Some(render_aur_inner(
        ctx, crate_cfg, aur_cfg, crate_name, log,
    )?))
}

pub fn publish_to_aur(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "aur")?;

    let aur_cfg = publish
        .aur
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no aur config for '{}'", crate_name))?;

    let git_url = match aur_check_skip_and_resolve_git_url(ctx, aur_cfg, crate_name, log)? {
        Some(u) => u,
        None => return Ok(false),
    };

    // The skip / `if` / `skip_upload` gate was already evaluated above by
    // `aur_check_skip_and_resolve_git_url`, so render via the skip-unaware
    // inner — the same render the offline schema validator drives — to keep a
    // single source of truth for the emitted PKGBUILD/.SRCINFO. Reuse the
    // package name the inner already resolved so the `aur.name` template is not
    // re-rendered (and re-warned on) a second time.
    let AurRendered {
        pkgbuild,
        srcinfo,
        package_name,
    } = render_aur_inner(ctx, crate_cfg, aur_cfg, crate_name, log)?;

    // The .install filename for the on-disk write (the inner already folded the
    // PKGBUILD `install=` line into the body).
    let install_base = package_name
        .strip_suffix("-bin")
        .unwrap_or(&package_name)
        .to_string();
    let install_filename = format!("{}.install", install_base);
    let version = ctx.version().replace('-', "_");

    // Clone AUR repo, write PKGBUILD, commit, push.
    let tmp_dir = tempfile::tempdir().context("aur: create temp dir")?;
    let repo_path = tmp_dir.path();
    aur_clone_repo(ctx, aur_cfg, &git_url, repo_path, log)?;

    let output_dir = aur_resolve_output_dir(ctx, aur_cfg, repo_path, log)?;
    aur_write_package_files(
        &output_dir,
        &pkgbuild,
        &srcinfo,
        &install_filename,
        aur_cfg.install.as_deref(),
        log,
    )?;

    aur_commit_and_push(
        ctx,
        aur_cfg,
        repo_path,
        &package_name,
        &version,
        &git_url,
        log,
    )
}
