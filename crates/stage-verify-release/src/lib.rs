//! Opt-in post-release verification gate (`verify_release:`).
//!
//! Runs LAST in the release pipeline — after the release is created and every
//! publisher has run — and REPORTS post-publish defects across THREE
//! independently-toggleable checks:
//!
//! - **asset-existence** — every produced artifact has a matching uploaded
//!   asset on the published release ([`asset_check`]). The produced set is
//!   derived for free from `release_uploadable_kinds()` + the artifact
//!   registry (no new config); the published set is fetched live via
//!   [`anodizer_stage_release::fetch_published_asset_names`]. The expected
//!   set additionally includes the signature / certificate / SBOM asset
//!   names the resolved `signs:` / `sboms:` config demands
//!   ([`anodizer_stage_sign::expected_signature_assets`],
//!   [`anodizer_stage_sbom::expected_sbom_assets`]) — derived from config +
//!   the artifact set rather than from the producing stages' registrations,
//!   so a sign/SBOM stage that silently produced nothing still fails the
//!   gate with the exact missing names.
//! - **install smoke-test** — each Linux package is installed in a pinned
//!   container and `<bin> --version` is run ([`smoke`]). Skipped with a
//!   notice when Docker is unavailable.
//! - **libc ceiling** — no glibc-linked `.deb` may require a glibc newer than
//!   the configured floor ([`libc_check`]). musl binaries are skipped.
//!
//! ## Failure semantics
//!
//! Because the gate runs AFTER the irreversible publish, it can only REPORT:
//! a defect is logged with the specific artifact / package / version and the
//! stage returns an error so CI exits non-zero — but the wording is always
//! explicit that **the release IS published**; the gate never implies the
//! publish failed and never attempts to undo it. Each check is best-effort
//! and independent: a Docker-unavailable environment SKIPS the smoke-test
//! with a notice rather than hard-failing the whole gate; asset-existence and
//! libc-ceiling need neither Docker nor extra config and still run.
//!
//! Works in all three config modes (single-crate, workspace-lockstep,
//! workspace per-crate): the stage iterates EVERY published crate and
//! verifies that crate's produced artifacts / debs.

mod asset_check;
mod libc_check;
mod smoke;

pub use asset_check::{AssetDiff, diff_assets};
pub use libc_check::{
    GlibcVersion, LibcCheckOutcome, check_glibc_ceiling, check_glibc_requirements,
    max_glibc_requirement,
};
pub use smoke::{
    PackageType, SmokeJob, SmokeOutcome, build_smoke_argv, docker_available, docker_platform,
    run_smoke,
};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{CrateConfig, VerifyReleaseConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::Result;

/// The post-release verification gate stage.
pub struct VerifyReleaseStage;

/// The stage's canonical name (also the `--skip=` value).
const STAGE_NAME: &str = "verify-release";

/// Wording prefix that makes every reported defect unambiguous: the release
/// already shipped, so the operator must investigate the LIVE release rather
/// than re-running a publish.
const PUBLISHED_NOTE: &str = "the release IS published — investigate";

impl Stage for VerifyReleaseStage {
    fn name(&self) -> &str {
        STAGE_NAME
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let cfg = ctx.config.verify_release.clone();
        if !cfg.enabled {
            return Ok(());
        }
        if ctx.should_skip(STAGE_NAME) {
            return Ok(());
        }
        // The gate verifies a real, published release; dry-run / snapshot
        // runs never created one, so there is nothing to verify.
        if ctx.is_dry_run() || ctx.is_snapshot() {
            ctx.logger(STAGE_NAME)
                .verbose("dry-run/snapshot — no published release to verify");
            return Ok(());
        }

        let log = ctx.logger(STAGE_NAME);

        // Every crate that produced a release. In single-crate mode this is
        // one crate; in workspace modes it is all published crates — the gate
        // verifies each crate's produced artifacts / debs without siloing.
        let selected = ctx.options.selected_crates.clone();
        let crates: Vec<CrateConfig> = ctx
            .config
            .crates
            .iter()
            .filter(|c| c.release.is_some())
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        if crates.is_empty() {
            log.verbose("no crates with a release block; nothing to verify");
            return Ok(());
        }

        let mut issues: Vec<String> = Vec::new();
        // Emit the resolved install-smoke strategy once, the first time a smoke job
        // runs, so a CI operator can tell a slow copy path (dind without a shared
        // work dir) from a fast bind-mount path.
        let mut smoke_strategy_logged = false;

        // Resolve Docker availability once (smoke-test only).
        let smoke_enabled = cfg.install_smoke.is_some();
        let docker_ok = if smoke_enabled {
            smoke::docker_available()
        } else {
            false
        };
        if smoke_enabled && !docker_ok {
            log.status(
                "skipped install smoke-test — Docker unavailable \
                 (asset-existence and libc-ceiling still run)",
            );
        }

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("verify-release: create tokio runtime: {e}"))?;

        for crate_cfg in &crates {
            verify_one_crate(
                ctx,
                &log,
                &rt,
                &cfg,
                crate_cfg,
                smoke_enabled,
                docker_ok,
                &mut issues,
                &mut smoke_strategy_logged,
            )?;
        }

        if issues.is_empty() {
            // Record the clean verdict on the context so the end-of-pipeline
            // summary renders a passing verify-release row (a separate axis
            // from the publisher rows).
            ctx.verify_release = Some(anodizer_core::VerifyReleaseSummary { issues: Vec::new() });
            log.status("all post-publish checks passed");
            return Ok(());
        }

        for issue in &issues {
            log.warn(issue);
        }
        // Stamp the failing verdict BEFORE bailing so the summary (emit_summary
        // fires after this stage returns Err) reflects the defects instead of a
        // uniform false all-`succeeded` green. The publishes genuinely landed;
        // this records the SEPARATE post-publish verification failure.
        ctx.verify_release = Some(anodizer_core::VerifyReleaseSummary {
            issues: issues.clone(),
        });
        anyhow::bail!(
            "verify-release: post-publish verification found {} issue(s); {}:\n  - {}",
            issues.len(),
            PUBLISHED_NOTE,
            issues.join("\n  - ")
        )
    }
}

/// Resolve the binary name to version-check for a crate: the first build's
/// `binary`, falling back to the crate name (mirrors BuildConfig's documented
/// fallback).
fn crate_binary_name(crate_cfg: &CrateConfig) -> String {
    crate_cfg
        .builds
        .as_ref()
        .and_then(|b| b.first())
        .and_then(|b| b.binary.clone())
        .unwrap_or_else(|| crate_cfg.name.clone())
}

/// The produced (upload-candidate) asset NAMES for one crate.
///
/// Derived from the SAME canonical upload-candidate enumeration the release
/// stage uploads from (`collect_release_upload_candidates`), so the
/// `release.ids` filter and the binary-sign-intermediate exclusion are shared
/// and cannot drift: an artifact the release stage filtered OUT by `ids` is
/// not reported as a missing asset here. The asset name is resolved exactly as
/// the upload path resolves it — the custom destination name when set,
/// otherwise the file's basename.
fn produced_asset_names(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Vec<String> {
    let mut names: Vec<String> = anodizer_stage_release::collect_release_upload_candidates(
        ctx, crate_name, ids, exclude,
        // include_meta: the asset-existence check verifies the regular
        // release-uploadable set, not the optional metadata.json sidecar.
        false,
    )
    .into_iter()
    .filter_map(|(path, custom_name)| {
        custom_name.or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
    })
    .collect();
    names.sort();
    names.dedup();
    names
}

/// Asset names the published release must ALSO carry per the resolved
/// `signs:` / `sboms:` config, derived from config + the artifact set — NOT
/// from the producing stages' registrations.
///
/// This is the gate's defense against a configured stage silently producing
/// nothing: the v0.8.0 release shipped with zero signature assets and the
/// produced-vs-published diff passed, because the silently-skipped sign stage
/// had registered no `Signature` artifacts and the produced set is
/// registry-derived. Expectations here come from the config itself, so a
/// no-op sign/SBOM stage yields a precise missing-asset failure.
///
/// Intentional skips create no expectations (see the derivation modules for
/// the full waiver order): the run's own skip record is consulted first as
/// the authoritative account of what THIS run decided, with the config's
/// `if:` / `skip:` re-evaluated only as a fallback.
fn config_expected_asset_names(
    ctx: &Context,
    crate_name: &str,
    release_ids: Option<&[String]>,
    release_exclude: Option<&[String]>,
) -> Result<Vec<String>> {
    // `release_ids` mirrors the upload path's id-filter semantics: derived
    // artifacts inherit their SUBJECT's verdict (`matches_id_filter`), so a
    // signature/SBOM is expected exactly when the artifact it derives from
    // is uploaded.
    let mut names = anodizer_stage_sign::expected_signature_assets(ctx, crate_name, release_ids)?;
    names.extend(anodizer_stage_sbom::expected_sbom_assets(
        ctx,
        crate_name,
        release_ids,
    )?);
    // Apply the SAME `release.exclude` the upload path applies: a signature or
    // SBOM the operator deliberately excluded from THIS release is not a
    // missing asset. Without this an `exclude: ["*.sig"]` would make the gate
    // demand an asset the upload was configured never to attach.
    names.retain(|n| anodizer_core::artifact::name_passes_exclude_filter(n, release_exclude));
    names.sort();
    names.dedup();
    Ok(names)
}

#[allow(clippy::too_many_arguments)]
fn verify_one_crate(
    ctx: &Context,
    log: &StageLogger,
    rt: &tokio::runtime::Runtime,
    cfg: &VerifyReleaseConfig,
    crate_cfg: &CrateConfig,
    smoke_enabled: bool,
    docker_ok: bool,
    issues: &mut Vec<String>,
    smoke_strategy_logged: &mut bool,
) -> Result<()> {
    // The caller filters to crates carrying a release block; if absent there
    // is no published release to verify, so skip this crate rather than panic.
    let Some(release_cfg) = crate_cfg.release.as_ref() else {
        return Ok(());
    };

    // (a) asset-existence ---------------------------------------------------
    if cfg.assert_assets_enabled() {
        match rt.block_on(anodizer_stage_release::fetch_published_asset_names(
            ctx,
            release_cfg,
            crate_cfg,
        )) {
            Ok(Some(published)) => {
                let produced = produced_asset_names(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    release_cfg.exclude.as_deref(),
                );
                // Config-derived expectations (signatures / SBOMs). A
                // derivation error is itself a finding — never a silent pass.
                let derived = match config_expected_asset_names(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    release_cfg.exclude.as_deref(),
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        issues.push(format!(
                            "could not derive expected signature/SBOM assets \
                                 from config for crate '{}': {e:#}",
                            crate_cfg.name
                        ));
                        Vec::new()
                    }
                };
                let mut all_expected = produced.clone();
                all_expected.extend(derived);
                all_expected.sort();
                all_expected.dedup();

                let diff = diff_assets(&all_expected, &published);
                let produced_set: std::collections::BTreeSet<&str> =
                    produced.iter().map(String::as_str).collect();
                let (missing_produced, missing_derived): (Vec<&String>, Vec<&String>) = diff
                    .missing
                    .iter()
                    .partition(|name| produced_set.contains(name.as_str()));
                if !missing_produced.is_empty() {
                    issues.push(format!(
                        "{1} produced artifact(s) missing from the published \
                         release for crate '{0}': {2}",
                        crate_cfg.name,
                        missing_produced.len(),
                        missing_produced
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !missing_derived.is_empty() {
                    issues.push(format!(
                        "{1} signature/SBOM asset(s) required by the resolved \
                         signs/sboms config were never uploaded for crate '{0}' \
                         (the producing stage registered no such artifact): {2}",
                        crate_cfg.name,
                        missing_derived.len(),
                        missing_derived
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !diff.has_missing() {
                    log.verbose(&format!(
                        "crate '{}' all {} asset(s) present \
                         ({} config-derived)",
                        crate_cfg.name,
                        all_expected.len(),
                        all_expected.len() - produced.len()
                    ));
                }
                if !diff.orphan.is_empty() {
                    log.verbose(&format!(
                        "crate '{}' {} orphan asset(s) on release (advisory): {}",
                        crate_cfg.name,
                        diff.orphan.len(),
                        diff.orphan.join(", ")
                    ));
                }
            }
            Ok(None) => {
                log.verbose(&format!(
                    "skipped asset-existence for crate '{}' — \
                     no GitHub release configured",
                    crate_cfg.name
                ));
            }
            Err(e) => {
                // Failing to fetch the live release is itself a post-publish
                // signal worth surfacing, not a silent skip.
                issues.push(format!(
                    "could not fetch published release assets for crate '{}': {e}",
                    crate_cfg.name
                ));
            }
        }
    }

    // (c) libc-ceiling ------------------------------------------------------
    // `glibc_check_enabled()` is true only when a ceiling is set; the
    // `if let` keeps that an invariant the type system enforces rather than an
    // unwrap that could panic if the predicate ever diverges from the field.
    if cfg.glibc_check_enabled()
        && let Some(ceiling) = cfg.glibc_ceiling.as_deref()
    {
        for (path, name, _) in linux_packages(ctx, &crate_cfg.name) {
            if !name.to_ascii_lowercase().ends_with(".deb") {
                continue;
            }
            check_one_deb_libc(log, &crate_cfg.name, path, ceiling, issues);
        }
    }

    // (b) install smoke-test ------------------------------------------------
    // `smoke_enabled` is derived from `install_smoke.is_some()`; the `if let`
    // ties the config presence to its enablement flag without an unwrap.
    if smoke_enabled
        && docker_ok
        && let Some(smoke_cfg) = cfg.install_smoke.as_ref()
    {
        let binary = crate_binary_name(crate_cfg);
        for (path, name, target) in linux_packages(ctx, &crate_cfg.name) {
            let Some(pt) = PackageType::from_filename(&name) else {
                continue;
            };
            let image = match pt {
                PackageType::Deb => smoke_cfg.deb_image(),
                PackageType::Rpm => smoke_cfg.rpm_image(),
                PackageType::Apk => smoke_cfg.apk_image(),
            };
            let job = SmokeJob {
                image: image.to_string(),
                package_type: pt,
                host_pkg_path: path.to_string_lossy().to_string(),
                pkg_name: name.clone(),
                binary: binary.clone(),
                platform: smoke::job_platform(target.as_deref()),
            };
            if !*smoke_strategy_logged {
                log.verbose(&format!(
                    "using install-smoke strategy {}",
                    smoke::strategy_label(&job.image)
                ));
                *smoke_strategy_logged = true;
            }
            match smoke::run_smoke(&job) {
                Ok(SmokeOutcome::Passed) => {
                    log.verbose(&format!(
                        "crate '{}' smoke passed ({name} on {image})",
                        crate_cfg.name
                    ));
                }
                Ok(SmokeOutcome::Failed { detail }) => {
                    issues.push(format!(
                        "install smoke-test failed for crate '{}' ({name} on {image}): {detail}",
                        crate_cfg.name
                    ));
                }
                Err(e) => {
                    issues.push(format!(
                        "install smoke-test could not run for crate '{}' ({name}): {e}",
                        crate_cfg.name
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Run the libc-ceiling check on one `.deb`'s embedded ELF binary.
fn check_one_deb_libc(
    log: &StageLogger,
    crate_name: &str,
    deb_path: std::path::PathBuf,
    ceiling: &str,
    issues: &mut Vec<String>,
) {
    let elf_bytes = match extract_deb_main_elf(&deb_path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            log.verbose(&format!(
                "skipped libc check for crate '{crate_name}' {} — \
                 has no inspectable ELF",
                deb_path.display()
            ));
            return;
        }
        Err(e) => {
            issues.push(format!(
                "could not read {} of crate '{crate_name}' for the libc check: {e}",
                deb_path.display()
            ));
            return;
        }
    };
    match libc_check::check_glibc_ceiling(&elf_bytes, ceiling) {
        Ok(LibcCheckOutcome::NoGlibcRequirement) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} has no glibc requirement \
                 (static/musl) — skipped",
                deb_path.display()
            ));
        }
        Ok(LibcCheckOutcome::WithinCeiling { max }) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} requires glibc {max} (<= {ceiling})",
                deb_path.display()
            ));
        }
        Ok(LibcCheckOutcome::ExceedsCeiling { max, ceiling }) => {
            issues.push(format!(
                "{} of crate '{crate_name}' requires glibc {max}, exceeding the \
                 configured ceiling {ceiling}",
                deb_path.display()
            ));
        }
        Err(e) => {
            issues.push(format!(
                "libc check failed for {} of crate '{crate_name}': {e}",
                deb_path.display()
            ));
        }
    }
}

/// All Linux-package artifacts for a crate as `(absolute_path, basename,
/// build_target)`.
///
/// The path is canonicalized (falling back to the registered path) so both
/// consumers work: the libc check reads the file, and the smoke-test
/// bind-mounts it into a container (which requires an absolute host path).
/// The target triple (when the package was built for one) lets the smoke-test
/// pin its container to the package's architecture. Callers filter by
/// extension at the call site.
fn linux_packages(
    ctx: &Context,
    crate_name: &str,
) -> Vec<(std::path::PathBuf, String, Option<String>)> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::LinuxPackage, crate_name)
        .into_iter()
        .map(|a| {
            let abs = std::fs::canonicalize(&a.path).unwrap_or_else(|_| a.path.clone());
            (abs, a.name.clone(), a.target.clone())
        })
        .collect()
}

/// Extract the largest executable ELF from a `.deb`'s `data.tar.*` payload.
///
/// A `.deb` is an `ar` archive containing `data.tar.{gz,xz,zst}`; the shipped
/// binary lives under `usr/bin/` (or similar). We pick the largest ELF member
/// as the binary to glibc-check — the common single-binary case. Returns
/// `Ok(None)` when no ELF is found (e.g. a data-only package).
///
/// Extraction is intentionally minimal and dependency-free: it scans the
/// `data.tar.*` for ELF members by magic bytes. When the payload is
/// compressed with a codec not linked into this build it returns `Ok(None)`
/// rather than erroring — the libc check is best-effort.
fn extract_deb_main_elf(deb_path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let bytes = std::fs::read(deb_path)?;
    let Some(data_tar) = deb::find_data_tar(&bytes)? else {
        return Ok(None);
    };
    Ok(deb::largest_elf_in_tar(&data_tar))
}

mod deb;

#[cfg(test)]
mod tests;

/// Environment requirements for the verify-release stage: the `docker` CLI
/// plus a reachable daemon when the install smoke-test is configured (the
/// only part of the gate that shells out).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let vr = &ctx.config.verify_release;
    if !vr.enabled || vr.install_smoke.is_none() {
        return Vec::new();
    }
    vec![
        anodizer_core::EnvRequirement::Tool {
            name: "docker".to_string(),
        },
        anodizer_core::EnvRequirement::DockerDaemon,
    ]
}
