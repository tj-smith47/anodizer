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

pub struct NfpmStage;

/// Render an `Option<String>` field in place through the template engine.
///
/// `None` is a no-op. Saves ~3 lines per field at the ~15 call sites where
/// nfpm field-by-field templating used to expand the same `if let Some(ref
/// s) = X { X = Some(ctx.render_template(s)?); }` shape inline.
fn render_in_place(field: &mut Option<String>, ctx: &mut Context) -> Result<()> {
    if let Some(s) = field.as_deref() {
        *field = Some(ctx.render_template(s)?);
    }
    Ok(())
}

/// A fully-staged nfpm job: config YAML written, filename decided,
/// subprocess args composed. Step 1 (serial, `&mut ctx`) renders all
/// templates and writes the YAML into `_tmp_dir`; Step 2 (parallel)
/// runs `nfpm pkg --packager <format>`. `_tmp_dir` keeps the config
/// file alive until the worker thread finishes.
struct NfpmJob {
    _tmp_dir: tempfile::TempDir,
    pkg_path: std::path::PathBuf,
    format: String,
    cmd_args: Vec<String>,
    /// Pre-parsed mtime for reproducible-build mtime stamping, or None
    /// when the config leaves `mtime` unset.
    mtime: Option<std::time::SystemTime>,
    mtime_repr: Option<String>,
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
            .crates
            .iter()
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

    let nfpm_artifact_kinds = &[
        ArtifactKind::Binary,
        ArtifactKind::Header,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
    ];
    let linux_binaries: Vec<_> = ctx
        .artifacts
        .by_kinds_and_crate(nfpm_artifact_kinds, &krate.name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_nfpm_target)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

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

        for (target, binary_paths, lib_paths) in &platform_groups {
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
) -> Result<()> {
    validate_format(format).with_context(|| format!("nfpm config for crate {}", crate_name))?;

    let Some((os, arch)) = resolve_format_os_arch(base_os, base_arch, format, log) else {
        return Ok(());
    };

    if let Some(triple) = target.as_deref()
        && !is_arch_supported_for_format(triple, format)
    {
        ctx.strict_guard(
            log,
            &format!(
                "nfpm: skipping format '{}' for target '{}': architecture not supported",
                format, triple
            ),
        )?;
        return Ok(());
    }

    let mut rendered_cfg = render_nfpm_config_fields(nfpm_cfg, ctx, crate_name)?;

    process_templated_contents(&mut rendered_cfg, nfpm_cfg, ctx, dist, crate_name, dry_run)?;
    process_templated_scripts(&mut rendered_cfg, nfpm_cfg, ctx, dist, crate_name, dry_run)?;

    fill_deb_arch_variant(&mut rendered_cfg, linux_binaries, target.as_deref());

    let pkg_name_owned = resolve_pkg_name(nfpm_cfg, &ctx.config.project_name, crate_name);
    let pkg_name: &str = pkg_name_owned.as_str();
    let ext = format_extension(format);

    setup_lintian_overrides(&mut rendered_cfg, format, pkg_name, &arch, dist, dry_run)?;

    let yaml_content = generate_nfpm_yaml_with_env(
        &rendered_cfg,
        version,
        binary_paths,
        Some(format),
        skip_sign,
        lib_paths,
        ctx.template_vars().all_env(),
    )?;

    let output_dir = dist.join("linux");
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

    let mut pkg_metadata = HashMap::from([("format".to_string(), format.to_string())]);
    if let Some(ref id) = nfpm_cfg.id {
        pkg_metadata.insert("id".to_string(), id.clone());
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) would run: nfpm pkg --packager {format} for crate {} target {:?}",
            crate_name, target
        ));
        new_artifacts.push(Artifact {
            kind: ArtifactKind::LinuxPackage,
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

/// Return the file extension for a given nfpm packager format.
pub(crate) fn format_extension(format: &str) -> &str {
    match format {
        "deb" | "termux.deb" => ".deb",
        "rpm" => ".rpm",
        "apk" => ".apk",
        "archlinux" => ".pkg.tar.zst",
        "ipk" => ".ipk",
        _ => "",
    }
}

/// Emit a Debian `lintian` override file and inject the matching content
/// entry into the rendered nfpm config, then clear the now-orphaned
/// `lintian_overrides:` field so the YAML output stays clean.
///
/// GoReleaser's `setupLintian` (`internal/pipe/nfpm/nfpm.go:601-623`)
/// writes a file to `<dist>/<format>/<package>_<arch>/lintian` whose body
/// is one `<package>: <override>` line per entry in `deb.lintian_overrides`,
/// then appends a `Content` mapping that path into the package at
/// `/usr/share/lintian/overrides/<package>` (mode 0644, packager-scoped to
/// `"deb"`). Anodizer previously parsed `deb.lintian_overrides` into a YAML
/// key but `nfpm` itself does not consume that key, so the override file
/// was silently dropped from the resulting `.deb` / `termux.deb`.
///
/// This helper performs the file emission and content injection in
/// lockstep with GR. When `dry_run` is true the on-disk write is skipped
/// (the content entry is still injected so the generated YAML reflects
/// what would ship). The helper is a no-op for non-deb formats and for
/// configs where `lintian_overrides` is unset / empty.
///
/// Returns an error only when the on-disk write fails — a configured
/// override list always reaches the `contents:` array.
pub(crate) fn setup_lintian_overrides(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    format: &str,
    pkg_name: &str,
    arch: &str,
    dist: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    if format != "deb" && format != "termux.deb" {
        return Ok(());
    }
    let Some(deb_cfg) = rendered_cfg.deb.as_mut() else {
        return Ok(());
    };
    let Some(overrides) = deb_cfg.lintian_overrides.take() else {
        return Ok(());
    };
    if overrides.is_empty() {
        return Ok(());
    }

    let pkg_dir = dist.join(format).join(format!("{pkg_name}_{arch}"));
    let lintian_path = pkg_dir.join("lintian");
    if !dry_run {
        fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("nfpm lintian: create dir {}", pkg_dir.display()))?;
        let body: String = overrides
            .iter()
            .map(|ov| format!("{pkg_name}: {ov}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&lintian_path, body)
            .with_context(|| format!("nfpm lintian: write {}", lintian_path.display()))?;
    }

    let entry = anodizer_core::config::NfpmContent {
        src: lintian_path.to_string_lossy().into_owned(),
        dst: format!("/usr/share/lintian/overrides/{pkg_name}"),
        content_type: None,
        file_info: Some(anodizer_core::config::NfpmFileInfo {
            owner: None,
            group: None,
            mode: Some(anodizer_core::config::StringOrU32(0o644)),
            mtime: None,
        }),
        packager: Some("deb".to_string()),
        expand: None,
    };
    rendered_cfg
        .contents
        .get_or_insert_with(Vec::new)
        .push(entry);
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers — sliced out of `run` to keep the body navigable.
// ---------------------------------------------------------------------------

fn validate_unique_config_ids(crates: &[anodizer_core::config::CrateConfig]) -> Result<()> {
    let mut seen_ids = std::collections::HashSet::new();
    for krate in crates {
        if let Some(ref nfpm_configs) = krate.nfpms {
            for cfg in nfpm_configs {
                let id = cfg.id.as_deref().unwrap_or("default");
                if !seen_ids.insert(id.to_string()) {
                    bail!(
                        "nfpm: duplicate config ID '{}' (each nfpm config must have a unique ID)",
                        id
                    );
                }
            }
        }
    }
    Ok(())
}

/// Evaluate per-config skip predicates (`if`, empty `formats`) and maintainer warning.
///
/// Returns `Ok(true)` when the caller should `continue` (skip this config),
/// `Ok(false)` to proceed.
fn should_skip_nfpm_config(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    nfpm_id_for_log: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    let proceed = anodizer_core::config::evaluate_if_condition(
        nfpm_cfg.if_condition.as_deref(),
        &format!("nfpm config '{nfpm_id_for_log}'"),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        let reason = "`if` condition evaluated falsy".to_string();
        log.verbose(&format!(
            "skipping nfpm config '{}': {}",
            nfpm_id_for_log, reason
        ));
        ctx.remember_skip("nfpm", nfpm_id_for_log, &reason);
        return Ok(true);
    }

    if nfpm_cfg.formats.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "nfpm config '{}': no output formats configured, skipping",
                nfpm_id_for_log
            ),
        )?;
        return Ok(true);
    }

    let maintainer = nfpm_cfg.maintainer.as_deref().unwrap_or("");
    if maintainer.is_empty() {
        log.warn(&format!(
            "nfpm config '{}': maintainer is empty (required for deb packages)",
            nfpm_id_for_log
        ));
    }

    Ok(false)
}

/// Build the per-platform artifact groups for one nfpm config.
///
/// GoReleaser groups all artifacts by platform and emits ONE package per
/// platform containing ALL artifacts for that platform. Returns `None` when
/// the caller should skip the current nfpm config (ids filter matched
/// nothing but there were binaries to begin with).
#[allow(clippy::type_complexity)]
fn build_platform_groups(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    krate: &anodizer_core::config::CrateConfig,
    linux_binaries: &[Artifact],
    is_meta: bool,
    log: &anodizer_core::log::StageLogger,
) -> Option<Vec<(Option<String>, Vec<String>, NfpmLibraryPaths)>> {
    if is_meta {
        if linux_binaries.is_empty() {
            return Some(vec![(None, Vec::new(), NfpmLibraryPaths::default())]);
        }
        let mut seen = std::collections::HashSet::new();
        return Some(
            linux_binaries
                .iter()
                .filter(|b| {
                    let key = b.target.clone().unwrap_or_default();
                    seen.insert(key)
                })
                .map(|b| (b.target.clone(), Vec::new(), NfpmLibraryPaths::default()))
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
                wants.iter().any(|w| w == v)
            })
            .collect()
    } else {
        id_filtered
    };

    if filtered.is_empty() && !linux_binaries.is_empty() {
        let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
        log.warn(&format!(
            "nfpm config '{}': ids filter matched no binaries, skipping",
            nfpm_id
        ));
        return None;
    }

    if filtered.is_empty() {
        return Some(vec![(
            None,
            vec![format!("dist/{}", krate.name)],
            NfpmLibraryPaths::default(),
        )]);
    }

    struct PlatformArtifacts {
        binaries: Vec<String>,
        libs: NfpmLibraryPaths,
    }
    let mut groups: std::collections::BTreeMap<Option<String>, PlatformArtifacts> =
        std::collections::BTreeMap::new();
    for b in &filtered {
        let entry = groups
            .entry(b.target.clone())
            .or_insert_with(|| PlatformArtifacts {
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
            .map(|(t, pa)| (t, pa.binaries, pa.libs))
            .collect(),
    )
}

/// Resolve the effective `(os, arch)` for a packaging format, honoring the
/// iOS- and AIX-specific GoReleaser overrides. Returns `None` when the
/// current `(base_os, base_arch, format)` combination is unsupported (the
/// caller should `continue`).
fn resolve_format_os_arch(
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
                    "skipping ios for format '{}': only deb is supported",
                    format
                ));
                None
            }
        }
        "aix" => {
            if base_arch != "ppc64" {
                log.status(&format!(
                    "skipping aix/{}: only ppc64 is supported",
                    base_arch
                ));
                return None;
            }
            if format == "rpm" {
                Some(("aix7.2".to_string(), "ppc".to_string()))
            } else {
                log.status(&format!(
                    "skipping aix for format '{}': only rpm is supported",
                    format
                ));
                None
            }
        }
        _ => Some((base_os.to_string(), base_arch.to_string())),
    }
}

/// Clone the nfpm config and template-render every string field that
/// participates in the generated YAML. Project-level `metadata.*` fall back
/// values are applied before rendering when the per-config field is unset
/// (GoReleaser Pro parity for `metadata.homepage/license/description/maintainers`).
pub(crate) fn render_nfpm_config_fields(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    ctx: &mut Context,
    crate_name: &str,
) -> Result<anodizer_core::config::NfpmConfig> {
    let mut rendered_cfg = nfpm_cfg.clone();
    if rendered_cfg.description.is_none() {
        rendered_cfg.description = ctx
            .config
            .meta_description_for(crate_name)
            .map(str::to_string);
    }
    if rendered_cfg.maintainer.is_none() {
        rendered_cfg.maintainer = ctx
            .config
            .meta_first_maintainer_for(crate_name)
            .map(str::to_string);
    }
    if rendered_cfg.homepage.is_none() {
        rendered_cfg.homepage = ctx.config.meta_homepage_for(crate_name).map(str::to_string);
    }
    if rendered_cfg.license.is_none() {
        rendered_cfg.license = ctx.config.meta_license_for(crate_name).map(str::to_string);
    }
    render_in_place(&mut rendered_cfg.description, ctx)?;
    render_in_place(&mut rendered_cfg.maintainer, ctx)?;
    render_in_place(&mut rendered_cfg.homepage, ctx)?;
    render_in_place(&mut rendered_cfg.license, ctx)?;
    render_in_place(&mut rendered_cfg.vendor, ctx)?;
    render_in_place(&mut rendered_cfg.section, ctx)?;
    render_in_place(&mut rendered_cfg.priority, ctx)?;
    render_in_place(&mut rendered_cfg.changelog, ctx)?;
    render_in_place(&mut rendered_cfg.bindir, ctx)?;
    render_in_place(&mut rendered_cfg.mtime, ctx)?;

    if let Some(ref mut scripts) = rendered_cfg.scripts {
        render_in_place(&mut scripts.preinstall, ctx)?;
        render_in_place(&mut scripts.postinstall, ctx)?;
        render_in_place(&mut scripts.preremove, ctx)?;
        render_in_place(&mut scripts.postremove, ctx)?;
    }

    // Render signature key_file, key_name, AND key_passphrase for all
    // formats. Skipping key_passphrase would leave an unrendered `{{ .Env.X
    // }}` reaching the signing backend, which fails as "bad passphrase".
    if let Some(ref mut deb) = rendered_cfg.deb
        && let Some(ref mut sig) = deb.signature
    {
        render_in_place(&mut sig.key_file, ctx)?;
        render_in_place(&mut sig.key_passphrase, ctx)?;
    }
    if let Some(ref mut rpm) = rendered_cfg.rpm
        && let Some(ref mut sig) = rpm.signature
    {
        render_in_place(&mut sig.key_file, ctx)?;
        render_in_place(&mut sig.key_passphrase, ctx)?;
    }
    if let Some(ref mut apk) = rendered_cfg.apk
        && let Some(ref mut sig) = apk.signature
    {
        render_in_place(&mut sig.key_file, ctx)?;
        render_in_place(&mut sig.key_name, ctx)?;
        render_in_place(&mut sig.key_passphrase, ctx)?;
    }
    if let Some(ref mut libdirs) = rendered_cfg.libdirs {
        render_in_place(&mut libdirs.header, ctx)?;
        render_in_place(&mut libdirs.cshared, ctx)?;
        render_in_place(&mut libdirs.carchive, ctx)?;
    }

    if let Some(ref mut entries) = rendered_cfg.contents {
        for entry in entries.iter_mut() {
            entry.src = ctx.render_template(&entry.src)?;
            entry.dst = ctx.render_template(&entry.dst)?;
            if let Some(ref mut fi) = entry.file_info {
                render_in_place(&mut fi.owner, ctx)?;
                render_in_place(&mut fi.group, ctx)?;
                render_in_place(&mut fi.mtime, ctx)?;
            }
        }
    }

    Ok(rendered_cfg)
}

/// GoReleaser Pro `templated_contents`: render each entry's body through
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

    let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
    let tmpl_dir = dist.join("nfpm-tmp").join(crate_name).join(nfpm_id);
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

/// GoReleaser Pro `templated_scripts`: render each named lifecycle script
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

    let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
    let tmpl_dir = dist.join("nfpm-tmp").join(crate_name).join(nfpm_id);
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

/// Fill `deb.arch_variant` from the per-target artifact `amd64_variant`
/// metadata when the user has not set it explicitly.
fn fill_deb_arch_variant(
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

/// Resolve the package name following GoReleaser nfpm.go:68-70 precedence:
/// explicit `package_name`, then project-level `project_name`, then the
/// crate name as last-resort fallback.
fn resolve_pkg_name(
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

/// Populate per-package template variables (`Os`, `Arch`, `Target`,
/// `Format`, `PackageName`, `ConventionalExtension`,
/// `ConventionalFileName`, `Release`, `Epoch`) before rendering the
/// user's `file_name_template`.
#[allow(clippy::too_many_arguments)]
fn set_nfpm_per_pkg_template_vars(
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
    ctx.template_vars_mut().set("Os", os);
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    ctx.template_vars_mut().set("Format", format);
    ctx.template_vars_mut().set("PackageName", pkg_name);
    ctx.template_vars_mut().set("ConventionalExtension", ext);
    let fn_info = filename::FileNameInfo::from_config(nfpm_cfg, pkg_name, version, arch);
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
fn compute_pkg_filename(
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
fn build_nfpm_job(
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
            .unwrap_or_else(|_| raw_mtime.clone());
        match anodizer_core::util::parse_mod_timestamp(&rendered_mtime) {
            Ok(mt) => (Some(mt), Some(rendered_mtime)),
            Err(e) => {
                log.warn(&format!("nfpm: invalid mtime '{rendered_mtime}': {e}"));
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    Ok(NfpmJob {
        _tmp_dir: tmp_dir,
        pkg_path: pkg_path.to_path_buf(),
        format: format.to_string(),
        cmd_args,
        mtime,
        mtime_repr,
        target: target.map(str::to_string),
        crate_name: crate_name.to_string(),
        pkg_metadata,
    })
}

/// Clear the per-target + per-packaging template variables once all jobs
/// have been prepared, so leaked state doesn't reach downstream stages
/// like `announce` or `publish`.
fn clear_nfpm_template_vars(ctx: &mut Context) {
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
fn execute_nfpm_jobs(
    jobs: &[NfpmJob],
    parallelism: usize,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Vec<Artifact>> {
    let run_job = |job: &NfpmJob| -> Result<Artifact> {
        let thread_log = anodizer_core::log::StageLogger::new("nfpm", verbosity);

        thread_log.status(&format!("running: {}", job.cmd_args.join(" ")));

        let output = Command::new(&job.cmd_args[0])
            .args(&job.cmd_args[1..])
            .output()
            .with_context(|| {
                format!(
                    "execute nfpm for format {} (crate {} target {:?})",
                    job.format, job.crate_name, job.target
                )
            })?;
        thread_log.check_output(output, "nfpm")?;

        if let Some(mt) = job.mtime {
            if let Err(e) = anodizer_core::util::set_file_mtime(&job.pkg_path, mt) {
                thread_log.warn(&format!(
                    "nfpm: failed to apply mtime to {}: {}",
                    job.pkg_path.display(),
                    e
                ));
            } else if let Some(ref repr) = job.mtime_repr {
                thread_log.verbose(&format!(
                    "nfpm: applied mtime={repr} to {}",
                    job.pkg_path.display()
                ));
            }
        }

        Ok(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: String::new(),
            path: job.pkg_path.clone(),
            target: job.target.clone(),
            crate_name: job.crate_name.clone(),
            metadata: job.pkg_metadata.clone(),
            size: None,
        })
    };

    anodizer_core::parallel::run_parallel_chunks(jobs, parallelism, "nfpm", run_job)
}
