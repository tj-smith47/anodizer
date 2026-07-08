//! Opt-in post-release verification gate (`verify_release:`).
//!
//! Runs LAST in the release pipeline — after the release is created and every
//! publisher has run — and REPORTS post-publish defects across FOUR
//! independently-toggleable checks:
//!
//! - **asset existence + content** — every produced artifact has a matching
//!   uploaded asset on the published release, and every present asset's
//!   stored size/digest matches the local bytes ([`asset_check`]). The
//!   produced set is derived for free from `release_uploadable_kinds()` +
//!   the artifact registry (no new config); the published set is fetched
//!   live via [`anodizer_stage_release::fetch_published_assets`]. The
//!   expected set additionally includes the signature / certificate / SBOM
//!   asset names the resolved `signs:` / `sboms:` config demands
//!   ([`anodizer_stage_sign::expected_signature_assets`],
//!   [`anodizer_stage_sbom::expected_sbom_assets`]) — derived from config +
//!   the artifact set rather than from the producing stages' registrations,
//!   so a sign/SBOM stage that silently produced nothing still fails the
//!   gate with the exact missing names.
//! - **publisher landing checks** — every publisher that succeeded this run
//!   actually landed: published crate versions are visible on the crates.io
//!   sparse index, npm package versions answer a registry GET, and uploaded
//!   blob objects answer a `HEAD` through the upload's own store backend
//!   ([`landing`]).
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
mod landing;
mod libc_check;
mod smoke;

pub use asset_check::{AssetDiff, ContentVerdict, check_asset_content, diff_assets};
pub use landing::LandingProbes;
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

/// Publishers whose published surface this gate verifies. When EVERY one is
/// deselected by the active `--publishers`/`--skip` surface, this run
/// published nothing the gate can check, so the stage self-skips — the same
/// consumer-aware gating `signs:` uses ([`anodizer_stage_sign::signs_consumers`]).
///
/// Each check axis is additionally gated on ITS OWN publisher: the asset
/// existence/content check consumes only github-release, and each landing
/// check fires only when its publisher's recorded outcome is `Succeeded` —
/// so a `--publishers npm` run still verifies the npm landing while skipping
/// the GitHub asset check.
pub fn verify_release_consumers() -> &'static [&'static str] {
    &["github-release", "cargo", "npm", "blob"]
}

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
        // A `--skip=github-release,cargo,npm,blob`-style run touched none of
        // the verifiable surfaces — nothing to verify here.
        if verify_release_consumers()
            .iter()
            .all(|p| ctx.publisher_deselected(p))
        {
            ctx.logger(STAGE_NAME)
                .status("skipped — no verifiable publisher in the selected publish surface");
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
            .crate_universe()
            .into_iter()
            .filter(|c| c.release.is_some())
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        if crates.is_empty() {
            // Landing checks are report-driven (a crate can publish to cargo
            // without a release block), so only the per-crate checks go quiet.
            log.verbose("no crates with a release block; no release assets to verify");
        }

        // The asset check consumes the GitHub Release surface only; a
        // `--publishers npm`-style run never touched it, so its assets are
        // out of the selected surface while the landing checks still apply.
        let github_selected = !ctx.publisher_deselected("github-release");
        if cfg.assert_assets_enabled() && !github_selected {
            log.verbose("github-release not in the selected publish surface — asset check skipped");
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
                github_selected,
                smoke_enabled,
                docker_ok,
                &mut issues,
                &mut smoke_strategy_logged,
            )?;
        }

        let mut landing_probed = 0usize;
        if cfg.landing_checks_enabled() {
            let policy = ctx.retry_policy();
            let cargo_probe = |name: &str, version: &str| {
                anodizer_stage_publish::cargo::published_on_crates_io(name, version, &policy, &log)
            };
            let npm_probe = |registry: &str, package: &str, version: &str| {
                anodizer_stage_publish::npm::version_visible_on_registry(
                    registry, package, version, &policy, &log,
                )
            };
            let blob_probe = |t: &anodizer_core::publish_evidence::BlobTargetSnapshot| {
                anodizer_stage_blob::blob_object_exists(ctx, t)
            };
            let probes = LandingProbes {
                cargo_index: &cargo_probe,
                npm_registry: &npm_probe,
                blob_head: &blob_probe,
            };
            landing_probed = landing::run_landing_checks(ctx, &log, &probes, &mut issues);
        }

        // Distinguish "everything verified" from "nothing was in scope to
        // verify": stamping a passing verdict when no check actually ran
        // would fabricate green evidence for a run that proved nothing.
        let asset_check_ran = cfg.assert_assets_enabled() && github_selected && !crates.is_empty();
        let libc_ran = cfg.glibc_check_enabled() && !crates.is_empty();
        let smoke_ran = smoke_enabled && docker_ok;
        let any_check_ran = asset_check_ran || libc_ran || smoke_ran || landing_probed > 0;
        if !any_check_ran && issues.is_empty() {
            log.verbose("no check ran against the selected publish surface — no verdict recorded");
            return Ok(());
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

/// Cap on the bytes the digest-fallback path will download and hash when
/// GitHub exposes no `sha256:` digest for an asset. Beyond this the check
/// stays honest but cheaper: size-only, with a verbose notice. 64 MiB covers
/// typical release binaries/archives without turning the gate into a full
/// re-download of multi-hundred-MB artifacts.
const DIGEST_DOWNLOAD_CAP: u64 = 64 * 1024 * 1024;

/// Per-crate rollup of the byte-level asset checks.
struct ContentSummary {
    /// Number of size/digest defects pushed into the issues vec.
    issue_count: usize,
    /// Assets whose size matched but whose digest could not be verified by
    /// any means (no `sha256:` digest served and too large to download).
    digest_unverified: usize,
}

/// Map every uploadable asset NAME of one crate to its local file path and
/// (when the checksum stage ran) its already-computed sha256.
///
/// Derived-asset names (signatures / SBOMs / renamed uploads) resolve through
/// the artifact registry first, then the release upload candidates so a
/// custom destination name maps to the same local file the upload read. An
/// expected name with no local entry (e.g. a config-derived expectation whose
/// producing stage never registered a file) simply gets no content check —
/// the existence diff already reports it.
fn local_asset_index(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
) -> std::collections::BTreeMap<String, (std::path::PathBuf, Option<String>)> {
    let mut idx = std::collections::BTreeMap::new();
    let sha_of = |path: &std::path::Path| {
        ctx.artifacts
            .all()
            .iter()
            .find(|a| a.path == path)
            .and_then(|a| a.metadata.get("sha256").cloned())
    };
    for a in ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.crate_name == crate_name)
    {
        idx.entry(a.name.clone())
            .or_insert_with(|| (a.path.clone(), a.metadata.get("sha256").cloned()));
    }
    for (path, custom_name) in anodizer_stage_release::collect_release_upload_candidates(
        ctx, crate_name, ids, exclude, false,
    ) {
        if let Some(name) = custom_name {
            let sha = sha_of(&path);
            idx.insert(name, (path, sha));
        }
    }
    idx
}

/// Compare each expected asset that IS present on the release against its
/// local bytes: stored size must equal the local file size, and the stored
/// `sha256:` digest (when GitHub serves one) must equal the local sha256.
/// When no digest is served, small assets are downloaded and hashed instead;
/// larger ones are verified by size only, with a verbose notice.
#[allow(clippy::too_many_arguments)]
fn verify_published_contents(
    ctx: &Context,
    log: &StageLogger,
    crate_cfg: &CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    expected: &[String],
    published: &[anodizer_stage_release::PublishedAsset],
    issues: &mut Vec<String>,
) -> ContentSummary {
    let local = local_asset_index(
        ctx,
        &crate_cfg.name,
        release_cfg.ids.as_deref(),
        release_cfg.exclude.as_deref(),
    );
    let mut summary = ContentSummary {
        issue_count: 0,
        digest_unverified: 0,
    };
    for name in expected {
        let Some(asset) = published.iter().find(|p| &p.name == name) else {
            continue;
        };
        let Some((path, meta_sha)) = local.get(name) else {
            log.verbose(&format!(
                "no local file registered for asset '{name}' — name-only check"
            ));
            continue;
        };
        let local_size = match std::fs::metadata(path) {
            Ok(md) => md.len(),
            Err(e) => {
                log.verbose(&format!(
                    "local file {} for asset '{name}' unreadable ({e}) — name-only check",
                    path.display()
                ));
                continue;
            }
        };
        // A checksum-stage sha256 is reused when present; otherwise the local
        // file is hashed here (cheap relative to the release it verifies).
        let local_sha = match meta_sha {
            Some(s) => s.clone(),
            None => match anodizer_core::hashing::sha256_file(path) {
                Ok(s) => s,
                Err(e) => {
                    issues.push(format!(
                        "could not hash local file {} for asset '{name}' of crate '{}': {e:#}",
                        path.display(),
                        crate_cfg.name
                    ));
                    summary.issue_count += 1;
                    continue;
                }
            },
        };
        match check_asset_content(local_size, &local_sha, asset.size, asset.digest.as_deref()) {
            ContentVerdict::Match => {
                log.verbose(&format!("asset '{name}' size+digest match"));
            }
            ContentVerdict::SizeMismatch { local, published } => {
                issues.push(format!(
                    "asset '{name}' of crate '{}' size mismatch: local {local} B vs \
                     published {published} B — the uploaded asset does not match the \
                     produced artifact",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
            }
            ContentVerdict::DigestMismatch { local, published } => {
                issues.push(format!(
                    "asset '{name}' of crate '{}' digest mismatch: local sha256 {local} \
                     vs published sha256 {published} — the uploaded asset does not \
                     match the produced artifact",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
            }
            ContentVerdict::DigestUnavailable => {
                if asset.size > DIGEST_DOWNLOAD_CAP {
                    log.verbose(&format!(
                        "asset '{name}' digest field unavailable and asset too large \
                         to download — verified size only"
                    ));
                    summary.digest_unverified += 1;
                    continue;
                }
                match download_sha256(
                    &asset.download_url,
                    ctx.options.token.as_deref(),
                    DIGEST_DOWNLOAD_CAP,
                ) {
                    Ok(remote_sha) if remote_sha.eq_ignore_ascii_case(&local_sha) => {
                        log.verbose(&format!(
                            "asset '{name}' digest verified via download (no digest field)"
                        ));
                    }
                    Ok(remote_sha) => {
                        issues.push(format!(
                            "asset '{name}' of crate '{}' digest mismatch (verified via \
                             download): local sha256 {local_sha} vs downloaded sha256 \
                             {remote_sha}",
                            crate_cfg.name
                        ));
                        summary.issue_count += 1;
                    }
                    Err(e) => {
                        issues.push(format!(
                            "could not download asset '{name}' of crate '{}' to verify \
                             its digest: {e:#}",
                            crate_cfg.name
                        ));
                        summary.issue_count += 1;
                    }
                }
            }
        }
    }
    summary
}

/// Download a release asset and return its sha256 hex, refusing to read more
/// than `cap` bytes — the digest fallback must never turn into an unbounded
/// re-download.
fn download_sha256(url: &str, token: Option<&str>, cap: u64) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(120))?;
    let mut req = client.get(url).header("Accept", "application/octet-stream");
    if let Some(token) = token {
        // reqwest strips the Authorization header on the cross-host redirect
        // GitHub issues to its storage backend, so the token never leaks to
        // the presigned URL host.
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send()?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned HTTP {status}");
    }
    let mut hasher = Sha256::new();
    let mut reader = resp.take(cap + 1);
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            anyhow::bail!("asset exceeds the {cap}-byte digest-download cap");
        }
        hasher.update(&buf[..n]);
    }
    Ok(anodizer_core::hashing::hex_lower(&hasher.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn verify_one_crate(
    ctx: &Context,
    log: &StageLogger,
    rt: &tokio::runtime::Runtime,
    cfg: &VerifyReleaseConfig,
    crate_cfg: &CrateConfig,
    github_selected: bool,
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

    // (a) asset existence + content ------------------------------------------
    if cfg.assert_assets_enabled() && github_selected {
        match rt.block_on(anodizer_stage_release::fetch_published_assets(
            ctx,
            release_cfg,
            crate_cfg,
        )) {
            Ok(Some(published_assets)) => {
                let published: Vec<String> =
                    published_assets.iter().map(|a| a.name.clone()).collect();
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

                // Every expected asset that IS present also gets a byte-level
                // check: stored size (and digest, when GitHub exposes one)
                // must match the local artifact.
                let content = verify_published_contents(
                    ctx,
                    log,
                    crate_cfg,
                    release_cfg,
                    &all_expected,
                    &published_assets,
                    issues,
                );
                let present = all_expected.len() - diff.missing.len();
                if !diff.has_missing() && content.issue_count == 0 {
                    let digest_note = match content.digest_unverified {
                        0 => "sizes+digests match".to_string(),
                        k => format!("sizes match ({k} digest(s) unverifiable)"),
                    };
                    log.status(&format!(
                        "github: crate '{}' {present}/{} assets present, {digest_note}",
                        crate_cfg.name,
                        all_expected.len(),
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
