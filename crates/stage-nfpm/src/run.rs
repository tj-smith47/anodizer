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

    let pkg_name_owned = resolve_pkg_name(nfpm_cfg, &ctx.config.project_name, crate_name);
    let pkg_name: &str = pkg_name_owned.as_str();
    let ext = format_extension(format);

    // Seed `Amd64` BEFORE rendering so a config field referencing `{{ .Amd64 }}`
    // (description/maintainer/conflicts/…) AND the `file_name_template` both see
    // this group's micro-arch variant. The conventional default filename
    // deliberately omits the variant (deb/rpm/apk require a bare `amd64` arch
    // field); the guard below is what stops two variants from colliding under
    // that default. `None`/`v1` seed empty, preserving single-variant names.
    anodizer_core::archive_name::seed_amd64_variant_var(ctx.template_vars_mut(), amd64_variant);

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
/// Lintian-override setup.
/// writes a file to `<dist>/<format>/<package>_<arch>/lintian` whose body
/// is one `<package>: <override>` line per entry in `deb.lintian_overrides`,
/// then appends a `Content` mapping that path into the package at
/// `/usr/share/lintian/overrides/<package>` (mode 0644, packager-scoped to
/// `"deb"`). Anodizer previously parsed `deb.lintian_overrides` into a YAML
/// key but `nfpm` itself does not consume that key, so the override file
/// was silently dropped from the resulting `.deb` / `termux.deb`.
///
/// This helper performs the file emission and content injection in
/// emitted in lockstep. When `dry_run` is true the on-disk write is skipped
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

/// Evaluate per-config skip predicates (`if`, empty `formats`).
///
/// Returns `Ok(true)` when the caller should `continue` (skip this config),
/// `Ok(false)` to proceed. The deb/apk maintainer requirement is enforced
/// later, per-format, in [`require_deb_apk_maintainer`] — only once the
/// Cargo-derived fallback has been applied and the format is known, so a
/// derivable maintainer or an rpm-only build doesn't trip it.
fn should_skip_nfpm_config(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    nfpm_id_for_log: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    if !nfpm_config_if_proceeds(ctx, nfpm_cfg, nfpm_id_for_log)? {
        let reason = "`if` condition evaluated falsy".to_string();
        log.verbose(&format!(
            "skipped nfpm config '{}' — {}",
            nfpm_id_for_log, reason
        ));
        ctx.remember_skip("nfpm", nfpm_id_for_log, &reason);
        return Ok(true);
    }

    if nfpm_cfg.formats.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "skipped nfpm config '{}' — no output formats configured",
                nfpm_id_for_log
            ),
        )?;
        return Ok(true);
    }

    Ok(false)
}

/// Returns `true` when a packaging format requires a non-empty `Maintainer`
/// to be installable. The deb family carries a mandatory `Maintainer` control
/// field:
///
/// - `deb` / `termux.deb` — Debian Policy 5.3 makes `Maintainer` mandatory;
///   lintian rejects an empty field and apt renders the package as "unknown".
/// - `apk` — Alpine's `APKINDEX` carries the maintainer the same way.
/// - `ipk` — the opkg control file is deb-derived and carries a `Maintainer`
///   line; nfpm warns and substitutes a placeholder when it is unset, so an
///   ipk with no maintainer ships incomplete metadata just like its deb sibling.
///
/// `rpm` and `archlinux` tolerate a missing packager differently (rpm's
/// `Packager` tag is optional; an Arch `.PKGINFO` has no required maintainer),
/// so they are not gated.
fn format_requires_maintainer(format: &str) -> bool {
    matches!(format, "deb" | "termux.deb" | "apk" | "ipk")
}

/// Resolve the effective maintainer for a crate's nfpm config: the explicit
/// `nfpm.maintainer`, else the first Cargo `authors` entry (via
/// `meta_first_maintainer_for`). Returns the trimmed value, or `None` when
/// neither source supplies one.
///
/// Mirrors the derivation order in [`render_nfpm_config_fields`] so the
/// pre-flight check and the rendered YAML agree on what the maintainer is.
fn resolve_effective_maintainer<'a>(
    config: &'a anodizer_core::config::Config,
    nfpm_cfg: &'a anodizer_core::config::NfpmConfig,
    crate_name: &str,
) -> Option<&'a str> {
    nfpm_cfg
        .maintainer
        .as_deref()
        .or_else(|| config.meta_first_maintainer_for(crate_name))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Hard-fail when a deb-family package (`deb`/`termux.deb`/`apk`/`ipk`) is
/// being built but no maintainer can be resolved — neither from
/// `nfpm.maintainer` nor a derivable Cargo `authors` entry. These formats all
/// carry a mandatory `Maintainer` control field; an empty one ships incomplete
/// metadata the repository index marks "unknown", so shipping it is a release
/// defect, not a warning. Scoped via [`format_requires_maintainer`]: an
/// rpm-only or archlinux-only build still succeeds.
///
/// This is a Rust-additive correctness improvement beyond GoReleaser (which
/// only warns), per the repo rule against advisory/continue-on-error on a
/// genuinely-broken output.
fn require_deb_apk_maintainer(
    config: &anodizer_core::config::Config,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    format: &str,
) -> Result<()> {
    if !format_requires_maintainer(format) {
        return Ok(());
    }
    if resolve_effective_maintainer(config, nfpm_cfg, crate_name).is_some() {
        return Ok(());
    }
    let id = nfpm_cfg.id.as_deref().unwrap_or("default");
    bail!(
        "nfpm config '{id}' builds a '{format}' package for crate '{crate_name}' but its \
         Maintainer field is empty and could not be derived. A '{format}' package with no \
         Maintainer ships incomplete metadata — the repository index marks it \"unknown\" \
         (and for deb, lintian rejects it). Set it via the `maintainer:` field on this nfpm \
         config (e.g. `maintainer: \"Jane Doe <jane@example.com>\"`) or add an `authors` \
         entry to the crate's Cargo.toml so anodizer can derive it."
    );
}

/// Evaluate one nfpm config's `if:` gate against the current template vars.
///
/// `Ok(true)` means the config proceeds; `Ok(false)` means a falsy `if:`
/// suppresses it (the build skips it, and the offline renderer emits no
/// YAML for it). Shared by the build's `should_skip_nfpm_config` and the
/// offline `nfpm_yaml_configs_for_crate` so a single render decides both.
fn nfpm_config_if_proceeds(
    ctx: &Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    nfpm_id_for_log: &str,
) -> Result<bool> {
    anodizer_core::config::evaluate_if_condition(
        nfpm_cfg.if_condition.as_deref(),
        &format!("nfpm config '{nfpm_id_for_log}'"),
        |t| ctx.render_template(t),
    )
}

/// Collect the packaging-eligible artifacts for one crate: every Binary /
/// Header / CArchive / CShared artifact whose target triple nfpm can package
/// (`is_nfpm_target`). Both the build's `run` loop and the offline
/// `nfpm_yaml_configs_for_crate` renderer start from this exact set so the
/// validated (config × target × format) universe equals the built one.
fn nfpm_eligible_artifacts(ctx: &Context, crate_name: &str) -> Vec<Artifact> {
    let nfpm_artifact_kinds = &[
        ArtifactKind::Binary,
        ArtifactKind::Header,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
    ];
    ctx.artifacts
        .by_kinds_and_crate(nfpm_artifact_kinds, crate_name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_nfpm_target)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Build the per-platform artifact groups for one nfpm config.
///
/// One per-platform package group: `(target, amd64_variant, binary_paths,
/// library_paths)`. The amd64 micro-architecture variant is part of the key so
/// two amd64 builds of one triple (baseline + e.g. `v3`) form separate groups
/// and each emits its own package instead of silently clobbering.
type PlatformGroup = (
    Option<String>,
    Option<String>,
    Vec<String>,
    NfpmLibraryPaths,
);

/// All artifacts are grouped by platform and ONE package is emitted per
/// platform containing ALL artifacts for that platform. Returns `None` when
/// the caller should skip the current nfpm config (ids filter matched
/// nothing but there were binaries to begin with).
fn build_platform_groups(
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
                wants.iter().any(|w| w == v)
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

/// Resolve the effective `(os, arch)` for a packaging format, honoring the
/// iOS- and AIX-specific overrides. Returns `None` when the
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

/// Fill `deb.arch_variant` from the per-target artifact's `amd64_variant`
/// (GOAMD64 microarch) metadata when the user has not set it explicitly, so an
/// amd64 deb is tagged with the microarchitecture it was built for.
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

/// Resolve the package name following this precedence:
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

/// Populate the per-target template variables (`Os`, `Arch`, `Target`,
/// `Libc`) shared by every per-target field that renders for one
/// (config × target) iteration.
///
/// Called before `render_nfpm_config_fields` so `conflicts`/`provides`/
/// `replaces` resolve against THIS target, then again (transitively, via
/// `set_nfpm_per_pkg_template_vars`) before the filename template renders.
/// `Libc` is `musl`/`gnu` for the respective triples, empty when the target
/// has no libc concept.
fn set_nfpm_per_target_template_vars(
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
    set_nfpm_per_target_template_vars(ctx, os, arch, target);
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

        thread_log.verbose(&format!("running {}", job.cmd_args.join(" ")));

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

        let pkg_name = job
            .pkg_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.pkg_path.display().to_string());
        thread_log.status(&format!("packed {pkg_name}"));

        if let Some(mt) = job.mtime {
            if let Err(e) = anodizer_core::util::set_file_mtime(&job.pkg_path, mt) {
                thread_log.warn(&format!(
                    "failed to apply mtime to {}: {}",
                    job.pkg_path.display(),
                    e
                ));
            } else if let Some(ref repr) = job.mtime_repr {
                thread_log.verbose(&format!(
                    "applied mtime={repr} to {}",
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
    let Some(krate) = ctx.config.crates.iter().find(|c| c.name == crate_name) else {
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
    anodizer_core::archive_name::seed_amd64_variant_var(&mut vars, amd64_variant);

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
