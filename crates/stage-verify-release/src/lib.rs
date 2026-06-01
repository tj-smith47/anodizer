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
//!   [`anodizer_stage_release::fetch_published_asset_names`].
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
    PackageType, SmokeJob, SmokeOutcome, build_smoke_argv, docker_available, run_smoke,
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
                .verbose("verify-release: dry-run/snapshot — no published release to verify");
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
            log.verbose("verify-release: no crates with a release block; nothing to verify");
            return Ok(());
        }

        let mut issues: Vec<String> = Vec::new();

        // Resolve Docker availability once (smoke-test only).
        let smoke_enabled = cfg.install_smoke.is_some();
        let docker_ok = if smoke_enabled {
            smoke::docker_available()
        } else {
            false
        };
        if smoke_enabled && !docker_ok {
            log.status(
                "verify-release: Docker unavailable — skipping install smoke-test \
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
            )?;
        }

        if issues.is_empty() {
            log.status("verify-release: all post-publish checks passed");
            return Ok(());
        }

        for issue in &issues {
            log.warn(&format!("verify-release: {issue}"));
        }
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

/// The produced (upload-candidate) artifact NAMES for one crate, derived from
/// `release_uploadable_kinds()` + the artifact registry — the same source of
/// truth the release stage uploads from. Rule #11: zero new config, no
/// hand-maintained list.
fn produced_asset_names(ctx: &Context, crate_name: &str) -> Vec<String> {
    let mut names: Vec<String> = anodizer_core::artifact::release_uploadable_kinds()
        .iter()
        .flat_map(|&kind| ctx.artifacts.by_kind_and_crate(kind, crate_name))
        .map(|a| a.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
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
) -> Result<()> {
    let release_cfg = crate_cfg
        .release
        .as_ref()
        .expect("filtered to crates with a release block");

    // (a) asset-existence ---------------------------------------------------
    if cfg.assert_assets_enabled() {
        match rt.block_on(anodizer_stage_release::fetch_published_asset_names(
            ctx,
            release_cfg,
            crate_cfg,
        )) {
            Ok(Some(published)) => {
                let produced = produced_asset_names(ctx, &crate_cfg.name);
                let diff = diff_assets(&produced, &published);
                if diff.has_missing() {
                    issues.push(format!(
                        "crate '{}': {} produced artifact(s) missing from the published \
                         release: {}",
                        crate_cfg.name,
                        diff.missing.len(),
                        diff.missing.join(", ")
                    ));
                } else {
                    log.verbose(&format!(
                        "verify-release: crate '{}' all {} asset(s) present",
                        crate_cfg.name,
                        produced.len()
                    ));
                }
                if !diff.orphan.is_empty() {
                    log.verbose(&format!(
                        "verify-release: crate '{}' {} orphan asset(s) on release (advisory): {}",
                        crate_cfg.name,
                        diff.orphan.len(),
                        diff.orphan.join(", ")
                    ));
                }
            }
            Ok(None) => {
                log.verbose(&format!(
                    "verify-release: crate '{}' no GitHub release configured — \
                     skipping asset-existence",
                    crate_cfg.name
                ));
            }
            Err(e) => {
                // Failing to fetch the live release is itself a post-publish
                // signal worth surfacing, not a silent skip.
                issues.push(format!(
                    "crate '{}': could not fetch published release assets: {e}",
                    crate_cfg.name
                ));
            }
        }
    }

    // (c) libc-ceiling ------------------------------------------------------
    if cfg.glibc_check_enabled() {
        let ceiling = cfg
            .glibc_ceiling
            .as_deref()
            .expect("glibc_check_enabled implies a ceiling");
        for (path, name) in linux_packages(ctx, &crate_cfg.name) {
            if !name.to_ascii_lowercase().ends_with(".deb") {
                continue;
            }
            check_one_deb_libc(log, &crate_cfg.name, path, ceiling, issues);
        }
    }

    // (b) install smoke-test ------------------------------------------------
    if smoke_enabled && docker_ok {
        let smoke_cfg = cfg
            .install_smoke
            .as_ref()
            .expect("smoke_enabled implies install_smoke is Some");
        let binary = crate_binary_name(crate_cfg);
        for (path, name) in linux_packages(ctx, &crate_cfg.name) {
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
            };
            match smoke::run_smoke(&job) {
                Ok(SmokeOutcome::Passed) => {
                    log.verbose(&format!(
                        "verify-release: crate '{}' smoke OK: {name} on {image}",
                        crate_cfg.name
                    ));
                }
                Ok(SmokeOutcome::Failed { detail }) => {
                    issues.push(format!(
                        "crate '{}': install smoke-test failed for {name} on {image}: {detail}",
                        crate_cfg.name
                    ));
                }
                Err(e) => {
                    issues.push(format!(
                        "crate '{}': install smoke-test could not run for {name}: {e}",
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
                "verify-release: crate '{crate_name}' {} has no inspectable ELF — \
                 skipping libc check",
                deb_path.display()
            ));
            return;
        }
        Err(e) => {
            issues.push(format!(
                "crate '{crate_name}': could not read {} for libc check: {e}",
                deb_path.display()
            ));
            return;
        }
    };
    match libc_check::check_glibc_ceiling(&elf_bytes, ceiling) {
        Ok(LibcCheckOutcome::NoGlibcRequirement) => {
            log.verbose(&format!(
                "verify-release: crate '{crate_name}' {} has no glibc requirement \
                 (static/musl) — skipped",
                deb_path.display()
            ));
        }
        Ok(LibcCheckOutcome::WithinCeiling { max }) => {
            log.verbose(&format!(
                "verify-release: crate '{crate_name}' {} requires glibc {max} (<= {ceiling})",
                deb_path.display()
            ));
        }
        Ok(LibcCheckOutcome::ExceedsCeiling { max, ceiling }) => {
            issues.push(format!(
                "crate '{crate_name}': {} requires glibc {max}, exceeding the configured \
                 ceiling {ceiling}",
                deb_path.display()
            ));
        }
        Err(e) => {
            issues.push(format!(
                "crate '{crate_name}': libc check failed for {}: {e}",
                deb_path.display()
            ));
        }
    }
}

/// All Linux-package artifacts for a crate as `(absolute_path, basename)`.
///
/// The path is canonicalized (falling back to the registered path) so both
/// consumers work: the libc check reads the file, and the smoke-test
/// bind-mounts it into a container (which requires an absolute host path).
/// Callers filter by extension at the call site.
fn linux_packages(ctx: &Context, crate_name: &str) -> Vec<(std::path::PathBuf, String)> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::LinuxPackage, crate_name)
        .into_iter()
        .map(|a| {
            let abs = std::fs::canonicalize(&a.path).unwrap_or_else(|_| a.path.clone());
            (abs, a.name.clone())
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
