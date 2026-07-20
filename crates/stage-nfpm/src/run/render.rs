use super::*;

/// Build the per-platform artifact groups for one nfpm config.
///
/// One per-platform package group: `(target, amd64_variant, binary_paths,
/// library_paths)`. The amd64 micro-architecture variant is part of the key so
/// two amd64 builds of one triple (baseline + e.g. `v3`) form separate groups
/// and each emits its own package instead of silently clobbering.
pub(crate) type PlatformGroup = (
    Option<String>,
    Option<String>,
    Vec<String>,
    NfpmLibraryPaths,
);

/// All artifacts are grouped by platform and ONE package is emitted per
/// platform containing ALL artifacts for that platform. Returns `None` when
/// the caller should skip the current nfpm config (ids filter matched
/// nothing but there were binaries to begin with).
pub(crate) fn build_platform_groups(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    krate: &anodizer_core::config::CrateConfig,
    linux_binaries: &[Artifact],
    is_meta: bool,
    log: &anodizer_core::log::StageLogger,
) -> Option<Vec<PlatformGroup>> {
    if is_meta {
        if linux_binaries.is_empty() {
            return Some(vec![(None, None, Vec::new(), NfpmLibraryPaths::default())]);
        }
        let mut seen = std::collections::HashSet::new();
        return Some(
            linux_binaries
                .iter()
                .filter(|b| {
                    let key = (
                        b.target.clone().unwrap_or_default(),
                        b.metadata.get("amd64_variant").cloned(),
                    );
                    seen.insert(key)
                })
                .map(|b| {
                    (
                        b.target.clone(),
                        b.metadata.get("amd64_variant").cloned(),
                        Vec::new(),
                        NfpmLibraryPaths::default(),
                    )
                })
                .collect(),
        );
    }

    // Apply ids filter
    let id_filtered: Vec<_> = if let Some(ref ids) = nfpm_cfg.ids {
        linux_binaries
            .iter()
            .filter(|b| {
                b.metadata
                    .get("id")
                    .map(|bid| ids.contains(bid))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        linux_binaries.iter().collect()
    };

    // `amd64_variant: []string` filter
    let filtered: Vec<_> = if let Some(ref wants) = nfpm_cfg.amd64_variant
        && !wants.is_empty()
    {
        id_filtered
            .into_iter()
            .filter(|b| {
                let target = b.target.as_deref().unwrap_or("");
                let (_, arch) = anodizer_core::target::map_target(target);
                if arch != "amd64" {
                    return true;
                }
                let v = b
                    .metadata
                    .get("amd64_variant")
                    .map(String::as_str)
                    .unwrap_or("v1");
                wants.iter().any(|w| w.as_str() == v)
            })
            .collect()
    } else {
        id_filtered
    };

    if filtered.is_empty() && !linux_binaries.is_empty() {
        let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
        log.warn(&format!(
            "skipped nfpm config '{}' — ids filter matched no binaries",
            nfpm_id
        ));
        return None;
    }

    if filtered.is_empty() {
        return Some(vec![(
            None,
            None,
            vec![format!("dist/{}", krate.name)],
            NfpmLibraryPaths::default(),
        )]);
    }

    struct PlatformArtifacts {
        binaries: Vec<String>,
        libs: NfpmLibraryPaths,
    }
    let mut groups: std::collections::BTreeMap<
        (Option<String>, Option<String>),
        PlatformArtifacts,
    > = std::collections::BTreeMap::new();
    for b in &filtered {
        let key = (b.target.clone(), b.metadata.get("amd64_variant").cloned());
        let entry = groups.entry(key).or_insert_with(|| PlatformArtifacts {
            binaries: Vec::new(),
            libs: NfpmLibraryPaths::default(),
        });
        let path = b.path.to_string_lossy().into_owned();
        match b.kind {
            ArtifactKind::Header => entry.libs.headers.push(path),
            ArtifactKind::CArchive => entry.libs.c_archives.push(path),
            ArtifactKind::CShared => entry.libs.c_shared.push(path),
            _ => entry.binaries.push(path),
        }
    }
    Some(
        groups
            .into_iter()
            .map(|((t, v), pa)| (t, v, pa.binaries, pa.libs))
            .collect(),
    )
}

/// Windows↔msix XOR gate: `msix` packages ONLY Windows binaries, and Windows
/// binaries package ONLY as `msix` — every other (platform, format) pairing
/// keeps its existing eligibility.
pub(crate) fn format_matches_platform(base_os: &str, format: &str) -> bool {
    (base_os == "windows") == (format == "msix")
}

/// Resolve the effective `(os, arch)` for a packaging format, honoring the
/// iOS- and AIX-specific overrides. Returns `None` when the
/// current `(base_os, base_arch, format)` combination is unsupported (the
/// caller should `continue`).
pub(crate) fn resolve_format_os_arch(
    base_os: &str,
    base_arch: &str,
    format: &str,
    log: &anodizer_core::log::StageLogger,
) -> Option<(String, String)> {
    match base_os {
        "ios" => {
            if format == "deb" {
                Some(("iphoneos-arm64".to_string(), base_arch.to_string()))
            } else {
                log.status(&format!(
                    "skipped ios for format '{}' — only deb is supported",
                    format
                ));
                None
            }
        }
        "aix" => {
            if base_arch != "ppc64" {
                log.status(&format!(
                    "skipped aix/{} — only ppc64 is supported",
                    base_arch
                ));
                return None;
            }
            if format == "rpm" {
                Some(("aix7.2".to_string(), "ppc".to_string()))
            } else {
                log.status(&format!(
                    "skipped aix for format '{}' — only rpm is supported",
                    format
                ));
                None
            }
        }
        _ => Some((base_os.to_string(), base_arch.to_string())),
    }
}

/// Set the per-target template vars, render the nfpm config for THIS target,
/// run the templated-contents/scripts + arch-variant + lintian passes, and
/// emit the final nfpm YAML string.
///
/// This is the single per-target render+generate path shared by the live and
/// dry-run branches of `process_nfpm_format`. The `set_nfpm_per_target_template_vars`
/// call here is load-bearing: it must run BEFORE `render_nfpm_config_fields`
/// so `conflicts`/`provides`/`replaces`/`recommends`/`suggests` resolve
/// `{{ .Libc }}` (and `Os`/`Arch`/`Target`) against this target. Removing it
/// would silently ship the literal template text.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_and_generate_nfpm_yaml(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    linux_binaries: &[Artifact],
    target: Option<&str>,
    binary_paths: &[String],
    lib_paths: &NfpmLibraryPaths,
    os: &str,
    arch: &str,
    format: &str,
    pkg_name: &str,
    dist: &std::path::Path,
    version: &str,
    skip_sign: bool,
    dry_run: bool,
) -> Result<String> {
    set_nfpm_per_target_template_vars(ctx, os, arch, target);

    let mut rendered_cfg =
        render_nfpm_config_fields(nfpm_cfg, &ctx.config, ctx.template_vars(), crate_name)?;
    default_nfpm_mtime_to_sde(&mut rendered_cfg, ctx.env_source());

    process_templated_contents(&mut rendered_cfg, nfpm_cfg, ctx, dist, crate_name, dry_run)?;
    process_templated_scripts(&mut rendered_cfg, nfpm_cfg, ctx, dist, crate_name, dry_run)?;
    pin_nfpm_script_mtimes(&mut rendered_cfg, nfpm_cfg, dist, crate_name, dry_run)?;

    fill_deb_arch_variant(&mut rendered_cfg, linux_binaries, target);

    setup_lintian_overrides(&mut rendered_cfg, format, pkg_name, arch, dist, dry_run)?;

    let render_target = crate::generate::NfpmRenderTarget {
        pkg_name,
        os,
        arch,
        target,
        format: Some(format),
        version,
        skip_sign,
    };
    generate_nfpm_yaml_with_env(
        &rendered_cfg,
        &render_target,
        binary_paths,
        lib_paths,
        ctx.template_vars().all_env(),
    )
}

/// Clone the nfpm config and template-render every string field that
/// participates in the generated YAML. Project-level `metadata.*` fall back
/// values are applied before rendering when the per-config field is unset
/// (fallback to `metadata.homepage/license/description/maintainers`, and the
/// crate's first author for `vendor`).
pub(crate) fn render_nfpm_config_fields(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    config: &anodizer_core::config::Config,
    vars: &anodizer_core::template::TemplateVars,
    crate_name: &str,
) -> Result<anodizer_core::config::NfpmConfig> {
    let mut rendered_cfg = nfpm_cfg.clone();
    if rendered_cfg.description.is_none() {
        rendered_cfg.description = config.meta_description_for(crate_name).map(str::to_string);
    }
    if rendered_cfg.maintainer.is_none() {
        rendered_cfg.maintainer = config
            .meta_first_maintainer_for(crate_name)
            .map(str::to_string);
    }
    if rendered_cfg.homepage.is_none() {
        rendered_cfg.homepage = config.meta_homepage_for(crate_name).map(str::to_string);
    }
    if rendered_cfg.license.is_none() {
        rendered_cfg.license = config.meta_license_for(crate_name).map(str::to_string);
    }
    if rendered_cfg.vendor.is_none() {
        // rpm/deb consumers expect a Vendor field (the distributing entity);
        // the crate's first author with its `<email>` stripped is the closest
        // accurate source, matching how a Debian/RPM Vendor is written.
        rendered_cfg.vendor = config.meta_vendor_for(crate_name);
    }
    render_in_place(&mut rendered_cfg.description, vars)?;
    render_in_place(&mut rendered_cfg.maintainer, vars)?;
    render_in_place(&mut rendered_cfg.homepage, vars)?;
    render_in_place(&mut rendered_cfg.license, vars)?;
    render_in_place(&mut rendered_cfg.vendor, vars)?;
    render_in_place(&mut rendered_cfg.section, vars)?;
    render_in_place(&mut rendered_cfg.priority, vars)?;
    render_in_place(&mut rendered_cfg.changelog, vars)?;
    render_in_place(&mut rendered_cfg.bindir, vars)?;
    render_in_place(&mut rendered_cfg.bin_alias, vars)?;
    render_in_place(&mut rendered_cfg.mtime, vars)?;

    // Render relationship lists per-target so a config can select a different
    // `Conflicts:`/`Provides:`/`Replaces:`/`Recommends:`/`Suggests:` per
    // libc/arch via `{{ .Libc }}` etc. These vars are set by
    // `set_nfpm_per_target_template_vars` before this function runs, so each
    // (config × target) iteration renders its own values.
    for list in [
        rendered_cfg.conflicts.as_mut(),
        rendered_cfg.provides.as_mut(),
        rendered_cfg.replaces.as_mut(),
        rendered_cfg.recommends.as_mut(),
        rendered_cfg.suggests.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for entry in list.iter_mut() {
            *entry = anodizer_core::template::render(entry, vars)?;
        }
    }

    if let Some(ref mut scripts) = rendered_cfg.scripts {
        render_in_place(&mut scripts.preinstall, vars)?;
        render_in_place(&mut scripts.postinstall, vars)?;
        render_in_place(&mut scripts.preremove, vars)?;
        render_in_place(&mut scripts.postremove, vars)?;
    }

    // Render signature key_file, key_name, AND key_passphrase for all
    // formats. Skipping key_passphrase would leave an unrendered `{{ .Env.X
    // }}` reaching the signing backend, which fails as "bad passphrase".
    if let Some(ref mut deb) = rendered_cfg.deb
        && let Some(ref mut sig) = deb.signature
    {
        render_in_place(&mut sig.key_file, vars)?;
        render_in_place(&mut sig.key_passphrase, vars)?;
    }
    if let Some(ref mut rpm) = rendered_cfg.rpm
        && let Some(ref mut sig) = rpm.signature
    {
        render_in_place(&mut sig.key_file, vars)?;
        render_in_place(&mut sig.key_passphrase, vars)?;
    }
    if let Some(ref mut apk) = rendered_cfg.apk {
        if let Some(ref mut sig) = apk.signature {
            render_in_place(&mut sig.key_file, vars)?;
            render_in_place(&mut sig.key_name, vars)?;
            render_in_place(&mut sig.key_passphrase, vars)?;
        }
        // apk's upgrade scripts are file paths like the top-level `scripts:`
        // entries, so they get the same `{{ .Env.* }}` render — otherwise an
        // unrendered path would reach nfpm literally.
        if let Some(ref mut scripts) = apk.scripts {
            render_in_place(&mut scripts.preupgrade, vars)?;
            render_in_place(&mut scripts.postupgrade, vars)?;
        }
    }
    // msix's identity/branding fields are templated (they carry
    // per-release values like the version or publisher), matching the set
    // GoReleaser runs through its template engine: publisher, the display
    // names, the logo path, and the signature's pfx_file.
    if let Some(ref mut msix) = rendered_cfg.msix {
        render_in_place(&mut msix.publisher, vars)?;
        if let Some(ref mut props) = msix.properties {
            render_in_place(&mut props.display_name, vars)?;
            render_in_place(&mut props.publisher_display_name, vars)?;
            render_in_place(&mut props.logo, vars)?;
        }
        if let Some(ref mut sig) = msix.signature {
            render_in_place(&mut sig.pfx_file, vars)?;
        }
    }

    if let Some(ref mut libdirs) = rendered_cfg.libdirs {
        render_in_place(&mut libdirs.header, vars)?;
        render_in_place(&mut libdirs.cshared, vars)?;
        render_in_place(&mut libdirs.carchive, vars)?;
    }

    if let Some(ref mut entries) = rendered_cfg.contents {
        for entry in entries.iter_mut() {
            entry.src = anodizer_core::template::render(&entry.src, vars)?;
            entry.dst = anodizer_core::template::render(&entry.dst, vars)?;
            if let Some(ref mut fi) = entry.file_info {
                render_in_place(&mut fi.owner, vars)?;
                render_in_place(&mut fi.group, vars)?;
                render_in_place(&mut fi.mtime, vars)?;
            }
        }
    }

    Ok(rendered_cfg)
}

/// Default the package `mtime` to `SOURCE_DATE_EPOCH` when the user leaves it
/// unset, so nfpm stamps reproducible archive-entry timestamps into the
/// .deb/.rpm payload instead of wall-clock.
///
/// Setting the top-level `mtime:` is the one knob that fixes the in-payload
/// bytes: it governs every content entry's mtime AND the RPM header's
/// BUILDTIME (verified empirically — an explicit `mtime` alone makes nfpm's
/// .rpm byte-identical across builds with no `SOURCE_DATE_EPOCH` in the
/// subprocess env). The post-build `set_file_mtime` only touches the outer
/// file's filesystem mtime, never the bytes. Doing it in anodizer (rather
/// than relying on nfpm's own env-`SOURCE_DATE_EPOCH` support) makes the
/// pin version-independent across nfpm releases.
///
/// Gated on SDE being present so non-harness production runs keep nfpm's
/// default behavior, mirroring the srpm stage's BUILDTIME clamp.
pub(crate) fn default_nfpm_mtime_to_sde(
    cfg: &mut anodizer_core::config::NfpmConfig,
    env: &dyn anodizer_core::env_source::EnvSource,
) {
    if cfg.mtime.is_none()
        && let Some(sde) = anodizer_core::sde::source_date_epoch_with_env(env)
    {
        cfg.mtime = Some(sde.to_rfc3339());
    }
}

/// `templated_contents`: render each entry's body through
/// Tera, write to a temp path under `dist/nfpm-tmp/<crate>/<nfpm_id>/`, and
/// append the rewritten entry to `contents`. User-supplied `dst` +
/// `file_info` are preserved; only `src` is rewritten.
fn process_templated_contents(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    ctx: &mut Context,
    dist: &std::path::Path,
    crate_name: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(templated_entries) = rendered_cfg.templated_contents.take() else {
        return Ok(());
    };
    if templated_entries.is_empty() {
        return Ok(());
    }

    let tmpl_dir = nfpm_tmp_dir(dist, crate_name, nfpm_cfg);
    if !dry_run {
        fs::create_dir_all(&tmpl_dir).with_context(|| {
            format!(
                "nfpm: create templated-contents dir: {}",
                tmpl_dir.display()
            )
        })?;
    }
    let rendered_contents = rendered_cfg.contents.get_or_insert_with(Vec::new);
    for (idx, mut entry) in templated_entries.into_iter().enumerate() {
        entry.src = ctx.render_template(&entry.src)?;
        entry.dst = ctx.render_template(&entry.dst)?;
        let body = fs::read_to_string(&entry.src)
            .with_context(|| format!("nfpm: read templated_contents src: {}", entry.src))?;
        let rendered_body = ctx
            .render_template(&body)
            .with_context(|| format!("nfpm: render templated_contents body for {}", entry.src))?;
        let base = std::path::Path::new(&entry.src)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("tmpl-{idx}"));
        let out_path = tmpl_dir.join(format!("{idx:03}-{base}"));
        if !dry_run {
            fs::write(&out_path, rendered_body.as_bytes()).with_context(|| {
                format!(
                    "nfpm: write rendered templated_contents: {}",
                    out_path.display()
                )
            })?;
        }
        entry.src = out_path.to_string_lossy().into_owned();
        rendered_contents.push(entry);
    }
    Ok(())
}

/// The per-config staging root `<dist>/nfpm-tmp/<crate>/<nfpm_id>` where
/// templated contents/scripts and pinned script copies live. An unnamed config
/// falls back to the `default` id.
fn nfpm_tmp_dir(
    dist: &std::path::Path,
    crate_name: &str,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
) -> std::path::PathBuf {
    let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
    dist.join("nfpm-tmp").join(crate_name).join(nfpm_id)
}

/// Stage every lifecycle script into an anodizer-owned dir with its mtime
/// pinned to the package's resolved `mtime`, then rewrite each script field to
/// the staged path.
///
/// nfpm's `mtime:` field normalizes script timestamps inside the deb/rpm
/// payloads but NOT inside an apk: the apk packager stamps each of its six
/// control scripts (the four top-level `scripts:` plus apk's `preupgrade`/
/// `postupgrade`) with the script file's filesystem mtime. The determinism
/// harness checks the script out in two separate hermetic worktrees, so git
/// sets two different checkout-time mtimes, the signed apk control segment
/// differs between the rebuilds, and the harness flags a false repro
/// regression. Copying each script to a pinned-mtime path before nfpm reads it
/// makes the apk byte-stable without mutating the user's working tree; deb/rpm
/// are unaffected (they normalize internally). No-op in dry-run, when no
/// scripts are set, or when `mtime` is unset/unparseable (the package is not
/// reproducible-by-config anyway, and `build_nfpm_job` surfaces the parse
/// warning).
pub(crate) fn pin_nfpm_script_mtimes(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    dist: &std::path::Path,
    crate_name: &str,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let Some(raw_mtime) = rendered_cfg.mtime.as_deref() else {
        return Ok(());
    };
    let Ok(mt) = anodizer_core::util::parse_mod_timestamp(raw_mtime) else {
        return Ok(());
    };

    let staged_dir = nfpm_tmp_dir(dist, crate_name, nfpm_cfg).join("scripts");
    // Create the dir lazily on the first script staged, so a config with no
    // scripts leaves no empty directory behind.
    let mut dir_ready = false;
    let mut stage = |name: &str, field: &mut Option<String>| -> Result<()> {
        let Some(src) = field.as_deref() else {
            return Ok(());
        };
        if !dir_ready {
            fs::create_dir_all(&staged_dir).with_context(|| {
                format!("nfpm: create script-pin dir: {}", staged_dir.display())
            })?;
            dir_ready = true;
        }
        let staged = staged_dir.join(format!("script-{name}"));
        fs::copy(src, &staged).with_context(|| {
            format!(
                "nfpm: stage script {name}: copy {src} -> {}",
                staged.display()
            )
        })?;
        anodizer_core::util::set_file_mtime(&staged, mt)
            .with_context(|| format!("nfpm: pin mtime on staged script {}", staged.display()))?;
        *field = Some(staged.to_string_lossy().into_owned());
        Ok(())
    };

    if let Some(scripts) = rendered_cfg.scripts.as_mut() {
        stage("preinstall", &mut scripts.preinstall)?;
        stage("postinstall", &mut scripts.postinstall)?;
        stage("preremove", &mut scripts.preremove)?;
        stage("postremove", &mut scripts.postremove)?;
    }
    if let Some(apk_scripts) = rendered_cfg.apk.as_mut().and_then(|a| a.scripts.as_mut()) {
        stage("preupgrade", &mut apk_scripts.preupgrade)?;
        stage("postupgrade", &mut apk_scripts.postupgrade)?;
    }
    Ok(())
}

/// `templated_scripts`: render each named lifecycle script
/// body and substitute the result into `rendered_cfg.scripts`. A templated
/// entry wins over a same-named plain `scripts` field.
fn process_templated_scripts(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    ctx: &mut Context,
    dist: &std::path::Path,
    crate_name: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(templated_scripts) = rendered_cfg.templated_scripts.take() else {
        return Ok(());
    };
    let any = templated_scripts.preinstall.is_some()
        || templated_scripts.postinstall.is_some()
        || templated_scripts.preremove.is_some()
        || templated_scripts.postremove.is_some();
    if !any {
        return Ok(());
    }

    let tmpl_dir = nfpm_tmp_dir(dist, crate_name, nfpm_cfg);
    if !dry_run {
        fs::create_dir_all(&tmpl_dir).with_context(|| {
            format!("nfpm: create templated-scripts dir: {}", tmpl_dir.display())
        })?;
    }
    let scripts_out = rendered_cfg
        .scripts
        .get_or_insert_with(NfpmScripts::default);
    let render_and_write = |name: &str, src_path: &str, ctx: &mut Context| -> Result<String> {
        let rendered_src = ctx.render_template(src_path)?;
        let body = fs::read_to_string(&rendered_src)
            .with_context(|| format!("nfpm: read templated_script {}: {}", name, rendered_src))?;
        let rendered_body = ctx
            .render_template(&body)
            .with_context(|| format!("nfpm: render templated_script {}: {}", name, rendered_src))?;
        let out_path = tmpl_dir.join(format!("script-{}", name));
        if !dry_run {
            fs::write(&out_path, rendered_body.as_bytes()).with_context(|| {
                format!(
                    "nfpm: write rendered templated_script: {}",
                    out_path.display()
                )
            })?;
        }
        Ok(out_path.to_string_lossy().into_owned())
    };
    if let Some(ref s) = templated_scripts.preinstall {
        scripts_out.preinstall = Some(render_and_write("preinstall", s, ctx)?);
    }
    if let Some(ref s) = templated_scripts.postinstall {
        scripts_out.postinstall = Some(render_and_write("postinstall", s, ctx)?);
    }
    if let Some(ref s) = templated_scripts.preremove {
        scripts_out.preremove = Some(render_and_write("preremove", s, ctx)?);
    }
    if let Some(ref s) = templated_scripts.postremove {
        scripts_out.postremove = Some(render_and_write("postremove", s, ctx)?);
    }
    Ok(())
}
