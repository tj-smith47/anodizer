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
//!   sparse index, npm package versions answer a registry GET, uploaded
//!   blob objects answer a `HEAD` through the upload's own store backend,
//!   and uploaded snaps are live in the Snap Store's public channel map —
//!   which also catches a manual-review hold that parked the revision
//!   outside every channel ([`landing`]).
//! - **install smoke-test** — each Linux package is installed in a pinned
//!   container and `<bin> --version` is run ([`smoke`]). Skipped with a
//!   notice when Docker is unavailable.
//! - **libc ceiling** — no glibc-linked binary shipped in a `.deb`, `.rpm`,
//!   or `.apk` may require a glibc newer than the configured floor
//!   ([`libc_check`]). musl binaries are skipped.
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
use anodizer_core::libc_check;
mod smoke;
mod snap_store;

pub use anodizer_core::libc_check::{
    GlibcVersion, LibcCheckOutcome, check_glibc_ceiling, check_glibc_requirements,
    max_glibc_requirement,
};
pub use asset_check::{AssetDiff, ContentVerdict, check_asset_content, diff_assets};
pub use landing::LandingProbes;
pub use smoke::{
    PackageType, SmokeJob, SmokeOutcome, build_smoke_argv, docker_available, docker_platform,
    run_smoke,
};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{CrateConfig, VerifyReleaseConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::publisher_kind::PublisherKind;
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

/// Publishers whose LANDING / asset surface this gate verifies. This list is
/// one of the stage's in-scope triggers, not the whole model: the stage runs
/// when ANY of (this list ∪ the OS-package carriers derived from
/// [`PublisherKind::carries_os_packages`] ∪ in-scope custom `publishers:`
/// entries that can carry OS packages) survives the active
/// `--publishers`/`--skip` surface — an `--publishers artifactory` run ships
/// installable packages, so the OS-package axes must still fire. Only when
/// NONE of those is selected did the run publish nothing the gate can check,
/// and the stage self-skips — the same consumer-aware gating `signs:` uses
/// ([`anodizer_stage_sign::signs_consumers`]).
///
/// Each check axis is additionally gated on ITS OWN publisher: the asset
/// existence/content check consumes only github-release, each landing
/// probe fires only when its publisher's recorded outcome is `Succeeded`
/// (a `Failed` attempt is reported as an issue directly, without a probe —
/// see [`landing`]), and the OS-package axes gate on
/// [`os_package_publisher_selected`] — so a `--publishers npm` run still
/// verifies the npm landing while skipping the GitHub asset check and the
/// package matrix.
pub fn verify_release_consumers() -> &'static [&'static str] {
    &[
        "github-release",
        "cargo",
        "npm",
        "blob",
        "snapcraft-publish",
    ]
}

/// Built-in publishers that can DELIVER installable OS packages
/// (`.deb`/`.rpm`/`.apk`) to users, derived from
/// [`PublisherKind::carries_os_packages`] so the set tracks the enum instead
/// of a hand-maintained string list. The OS-package verify axes
/// (install-smoke and libc-ceiling) verify those produced artifacts, so —
/// like the asset check gating on `github-release` — they run only when at
/// least one such publisher is in the selected publish surface.
/// Over-inclusion is safe (a surface that ships no package finds nothing to
/// check); under-inclusion would drop coverage on a shipped package.
pub fn os_package_consumers() -> Vec<&'static str> {
    PublisherKind::all()
        .filter(|k| k.carries_os_packages())
        .map(PublisherKind::token)
        .collect()
}

/// Whether the selected publish surface delivers any installable OS package,
/// i.e. whether the OS-package verify axes (install-smoke, libc-ceiling) have
/// anything in scope. A `--publishers npm` run ships no OS package, so those
/// axes are out of scope there; an in-scope custom `publishers:` entry that
/// can carry OS packages keeps them in scope just like a built-in carrier.
fn os_package_publisher_selected(ctx: &Context) -> bool {
    ctx.any_publisher_selected(&os_package_consumers()) || custom_os_package_publisher_selected(ctx)
}

/// Whether any configured custom `publishers:` entry that can carry an OS
/// package is selected this run.
///
/// A custom exec publisher carries OS packages when its `artifact_types`
/// filter is absent (the curated default set includes `linux_package`) or
/// explicitly lists `linux_package`. An entry is out of scope when its `cmd`
/// is empty, the operator deselected its effective name (the `name:` field,
/// falling back to the same `publisher[i]` index label the exec dispatch
/// uses), its `skip:` renders true, or its `if:` renders falsy. Template
/// evaluation is best-effort here: a render failure counts the entry as
/// selected, because over-inclusion only makes the axes look at packages the
/// run produced anyway, while under-inclusion would silently drop coverage.
fn custom_os_package_publisher_selected(ctx: &Context) -> bool {
    let Some(publishers) = ctx.config.publishers.as_ref() else {
        return false;
    };
    publishers.iter().enumerate().any(|(i, p)| {
        if p.cmd.is_empty() {
            return false;
        }
        let carries = p.artifact_types.as_ref().is_none_or(|types| {
            types
                .iter()
                .any(|t| t == ArtifactKind::LinuxPackage.as_str())
        });
        if !carries {
            return false;
        }
        let name = p.name.clone().unwrap_or_else(|| format!("publisher[{i}]"));
        if ctx.publisher_deselected(&name) {
            return false;
        }
        if let Some(skip) = p.skip.as_ref()
            && skip
                .try_evaluates_to_true(|s| ctx.render_template(s))
                .unwrap_or(false)
        {
            return false;
        }
        !matches!(
            anodizer_core::config::evaluate_if_condition(
                p.if_condition.as_deref(),
                &format!("publisher '{name}'"),
                |s| ctx.render_template(s),
            ),
            Ok(false)
        )
    })
}

impl Stage for VerifyReleaseStage {
    fn name(&self) -> &str {
        STAGE_NAME
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let cfg = ctx.config.verify_release.clone();
        if !cfg.enabled {
            ctx.logger(STAGE_NAME)
                .status("verify-release skipped: disabled by config");
            return Ok(());
        }
        if ctx.should_skip(STAGE_NAME) {
            return Ok(());
        }
        // A `--skip=github-release,cargo,npm,blob,...`-style run touched none
        // of the verifiable surfaces — nothing to verify here. OS-package
        // carriers (built-in or custom exec) count as verifiable: their
        // shipped packages are exactly what the install-smoke / libc-ceiling
        // axes exist to check.
        if !ctx.any_publisher_selected(verify_release_consumers())
            && !os_package_publisher_selected(ctx)
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

        let mut run_state = VerifyRun::default();

        // The install-smoke axis verifies produced OS packages (.deb/.rpm/.apk)
        // are installable — coverage owned by the publishers that DELIVER those
        // packages, so it is gated on its own surface exactly like the asset
        // check above. A `--publishers npm` run ships no OS package, so
        // re-running the cross-arch smoke matrix there is both redundant (the
        // OS-package publish job already ran it) and unrunnable (an arm64
        // package cannot exec on an x86_64 runner without qemu/binfmt).
        // The install-smoke and libc-ceiling axes both verify produced OS
        // packages the run ships, so both are gated on the OS-package surface.
        let os_pkg_selected = os_package_publisher_selected(ctx);
        let smoke_enabled = cfg.install_smoke.is_some();
        let smoke_in_surface = smoke_enabled && os_pkg_selected;
        // Probe Docker only when smoke is actually in scope; an out-of-surface
        // run has nothing to smoke, so the probe would be wasted work.
        let docker_ok = if smoke_in_surface {
            smoke::docker_available()
        } else {
            false
        };
        if smoke_enabled && !smoke_in_surface {
            // status-ok: operator-visible axis skip, same class as the
            // Docker-unavailable skip below — not a command echo.
            log.status(
                "skipped install smoke-test — out of the selected publish surface \
                 (no OS-package publisher selected)",
            );
        } else if smoke_enabled && !docker_ok {
            log.status(
                "skipped install smoke-test — Docker unavailable \
                 (asset-existence and libc-ceiling still run)",
            );
        }
        if cfg.glibc_check_enabled() && !os_pkg_selected {
            // status-ok: operator-visible axis skip, symmetric with the
            // smoke-axis skip above — not a command echo.
            log.status(
                "skipped libc-ceiling — out of the selected publish surface \
                 (no OS-package publisher selected)",
            );
        }

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("verify-release: create tokio runtime: {e}"))?;

        let scope = AxisScope {
            github_selected,
            os_pkg_selected,
            smoke_enabled,
            docker_ok,
        };
        let mut totals = CrateVerifyOutcome::default();
        for crate_cfg in &crates {
            let outcome =
                verify_one_crate(ctx, &log, &rt, &cfg, crate_cfg, &scope, &mut run_state)?;
            totals.absorb(&outcome);
        }
        let mut issues = run_state.issues;

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
            let snap_probe = |snap: &str, version: &str, channel: Option<&str>| {
                snap_store::snap_version_in_channel_map(snap, version, channel, &policy, &log)
            };
            let probes = LandingProbes {
                cargo_index: &cargo_probe,
                npm_registry: &npm_probe,
                blob_head: &blob_probe,
                snap_channel_map: &snap_probe,
            };
            landing_probed = landing::run_landing_checks(ctx, &log, &probes, &mut issues);
        }

        // Distinguish "everything verified" from "nothing was in scope to
        // verify": stamping a passing verdict when no check actually ran
        // would fabricate green evidence for a run that proved nothing.
        let any_check_ran = totals.any_inspected() || landing_probed > 0;
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

/// Pre-submitter verify-release gate: the asset existence + content axis of
/// this stage ([`verify_one_crate`]'s `github_selected` branch), runnable
/// standalone against an already-published release, without the landing /
/// smoke / libc axes (those verify a specific successful publisher's own
/// landing, not the asset surface every one-way-door publisher depends on).
///
/// Installed once into [`Context::verify_gate`](anodizer_core::context::Context::verify_gate)
/// by the CLI's pipeline-composition layer and invoked by `stage-publish`'s
/// dispatcher immediately before the first Submitter-group (one-way-door)
/// publisher would run. Returns `Ok(true)` when the check is out of scope
/// (disabled, dry-run/snapshot, or no crate has a release block) or ran
/// clean; `Ok(false)` when it ran and found asset-content defects — including
/// a fetch failure (e.g. no GitHub release exists for the tag), which
/// [`verify_one_crate`] records as an issue rather than propagating; `Err`
/// only for an unrecoverable setup failure (e.g. the tokio runtime could not
/// be created), which the dispatcher treats the same as `Ok(false)`: blocking
/// rather than a pass.
///
/// Deliberately does NOT auto-pass when `github-release` is deselected from
/// `--publishers` (e.g. the OIDC leg's `--publishers npm,pypi,cargo`): the
/// release and its assets were already published by an earlier leg, and the
/// immutable registries this gate guards depend on that content being
/// correct regardless of which leg re-verifies it.
///
/// Deliberately reuses [`verify_one_crate`] rather than re-implementing the
/// asset diff/content check: the same expected-vs-published/local-bytes
/// comparison the terminal [`VerifyReleaseStage`] runs, called with an
/// [`AxisScope`] that leaves the OS-package axes (libc-ceiling, install-smoke)
/// out of scope — those verify a specific publisher's own package landing,
/// not the asset surface every irreversible publisher depends on being
/// correct before it commits to an immutable registry.
///
/// Deliberately does NOT consult [`Context::should_skip`](anodizer_core::context::Context::should_skip)
/// for `verify-release`, unlike [`VerifyReleaseStage::run`]'s terminal-stage
/// check. `--skip=verify-release` suppresses the terminal post-publish
/// REPORT (nothing left to undo by then), but this gate runs BEFORE the
/// first one-way-door publisher — honoring the same skip here would let
/// `--skip=verify-release` silently reopen the burn-before-verify hole this
/// gate exists to close. The sanctioned bypass for a recovery run that needs
/// to proceed past a known-bad asset check is the explicit `--no-gate-submitter`
/// flag, which disables the gate hook entirely
/// ([`Context::verify_gate`](anodizer_core::context::Context::verify_gate) is
/// never installed / never consulted) rather than being routed through this
/// function's own skip handling.
pub fn run_asset_gate(ctx: &mut Context) -> Result<bool> {
    let cfg = ctx.config.verify_release.clone();
    if !cfg.enabled || !cfg.assert_assets_enabled() {
        ctx.logger(STAGE_NAME)
            .status("verify-release gate skipped: disabled by config");
        return Ok(true);
    }
    if ctx.is_dry_run() || ctx.is_snapshot() {
        ctx.logger(STAGE_NAME)
            .status("(dry-run) would verify release content before one-way-door publishers");
        return Ok(true);
    }

    let log = ctx.logger(STAGE_NAME);
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
        return Ok(true);
    }

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| anyhow::anyhow!("verify-release gate: create tokio runtime: {e}"))?;
    let scope = AxisScope {
        github_selected: true,
        os_pkg_selected: false,
        smoke_enabled: false,
        docker_ok: false,
    };
    let mut run_state = VerifyRun::default();
    for crate_cfg in &crates {
        verify_one_crate(ctx, &log, &rt, &cfg, crate_cfg, &scope, &mut run_state)?;
    }
    if run_state.issues.is_empty() {
        Ok(true)
    } else {
        for issue in &run_state.issues {
            log.warn(&format!("verify-release gate: {issue}"));
        }
        Ok(false)
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
) -> std::collections::BTreeMap<String, (std::path::PathBuf, Option<String>, ArtifactKind)> {
    let mut idx = std::collections::BTreeMap::new();
    let sha_and_kind_of = |path: &std::path::Path| {
        ctx.artifacts
            .all()
            .iter()
            .find(|a| a.path == path)
            .map(|a| (a.metadata.get("sha256").cloned(), a.kind))
    };
    for a in ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.crate_name == crate_name)
    {
        idx.entry(a.name.clone())
            .or_insert_with(|| (a.path.clone(), a.metadata.get("sha256").cloned(), a.kind));
    }
    for (path, custom_name) in anodizer_stage_release::collect_release_upload_candidates(
        ctx, crate_name, ids, exclude, false,
    ) {
        if let Some(name) = custom_name {
            let (sha, kind) = sha_and_kind_of(&path).unwrap_or((None, ArtifactKind::Archive));
            idx.insert(name, (path, sha, kind));
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
    let signature_suffixes = anodizer_core::signature_assets::signature_asset_suffixes(&ctx.config);
    let mut summary = ContentSummary {
        issue_count: 0,
        digest_unverified: 0,
    };
    // A locally-registered asset is classified by its exact `ArtifactKind`
    // (`Signature` / `Certificate`); an asset with no local entry (uploaded
    // from a prior run, or produced outside this invocation) falls back to
    // the suffix set, since no kind signal exists for it. The suffix set's
    // dynamic-tail templates can false-fail this fallback, but a false
    // digest-mismatch report is the safer failure mode than silently
    // exempting real content.
    let classify_signature = |name: &str| -> bool {
        match local.get(name) {
            Some((_, _, kind)) => {
                matches!(kind, ArtifactKind::Signature | ArtifactKind::Certificate)
            }
            None => anodizer_core::signature_assets::is_signature_asset(name, &signature_suffixes),
        }
    };
    // Cryptographic re-verification of signature assets, resolved lazily so
    // a release without a single present signature asset never spawns a
    // verifier, derives a public key, or downloads anything.
    let mut crypto: Option<anodizer_stage_sign::SignatureVerification> = None;
    for name in expected {
        let Some(asset) = published.iter().find(|p| &p.name == name) else {
            continue;
        };
        let is_signature = classify_signature(name);
        if is_signature {
            // GPG/cosign signatures embed a timestamp or random nonce, so a
            // resign of byte-identical input never reproduces the same
            // bytes — a digest comparison would flag every re-published
            // signature as "mismatched" even though nothing is wrong.
            // Presence is still enforced upstream in the missing-asset
            // diff; this only exempts the byte-level comparison.
            // This is a DELIBERATE, narrower exemption than it looks: it
            // applies only inside this `if is_signature` arm. Every other
            // (payload) asset falls through to the digest/size comparison
            // below — `is_signature_asset`/the exact `ArtifactKind` match
            // above are the only gate, so a payload asset can never take
            // this shortcut and skip its digest check.
            if asset.size == 0 {
                issues.push(format!(
                    "signature/certificate asset '{name}' of crate '{}' is empty (0 bytes) — \
                     the signing step likely failed silently",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
                continue;
            }
            // In place of the exempted digest comparison, the signature is
            // re-verified CRYPTOGRAPHICALLY against its payload with
            // material derived from the resolved `signs:` config (keyed
            // cosign public key, keyless identity, gpg keyring). The
            // PUBLISHED signature bytes are downloaded and verified against
            // the local payload (whose equality with the published payload
            // the digest check establishes), so a signature corrupted or
            // replaced on the release is caught; a failed download degrades
            // to verifying the locally-produced bytes. Only a POSITIVE
            // rejection fails; any environmental shortfall (tool or key
            // material absent in this leg, download unavailable) falls back
            // to the presence + non-empty check above.
            let verification = crypto.get_or_insert_with(|| {
                let download_dir = tempfile::Builder::new()
                    .prefix("anodizer-verify-sig-")
                    .tempdir();
                let source = match &download_dir {
                    Ok(dir) => published_signature_source(
                        &local,
                        published,
                        expected,
                        &classify_signature,
                        ctx.options.token.as_deref(),
                        dir.path(),
                        log,
                    ),
                    Err(e) => {
                        log.verbose(&format!(
                            "could not create a download dir for published signature \
                             bytes ({e}) — verifying locally-produced bytes instead"
                        ));
                        anodizer_stage_sign::PublishedSignatureSource::default()
                    }
                };
                anodizer_stage_sign::verify_signature_assets(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    &source,
                    log,
                )
            });
            match verification.outcome(name) {
                Some(anodizer_stage_sign::SignatureCryptoOutcome::Verified) => {
                    log.status(&format!(
                        "verified signature '{name}' (cryptographic check)"
                    ));
                }
                Some(anodizer_stage_sign::SignatureCryptoOutcome::Invalid(reason)) => {
                    issues.push(format!(
                        "signature/certificate asset '{name}' of crate '{}' FAILED \
                         cryptographic verification: {reason} — the signature does not \
                         verify against the artifact it signs",
                        crate_cfg.name
                    ));
                    summary.issue_count += 1;
                }
                None => {
                    log.verbose(&format!(
                        "asset '{name}' is a signature/certificate — present and non-empty, \
                         digest comparison exempted (no cryptographic verdict was derivable: \
                         the verifier tool, key material, or producing sign config is \
                         unavailable in this environment)"
                    ));
                }
            }
            continue;
        }
        let Some((path, meta_sha, _kind)) = local.get(name) else {
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

/// Size ceiling for downloading a published signature / certificate asset:
/// detached signatures, sigstore bundles, and PEM certificates are all far
/// below this; anything larger is not a signature worth pulling.
const SIGNATURE_DOWNLOAD_CAP: u64 = 4 * 1024 * 1024;

/// Build the published-bytes view the signature crypto check consumes: the
/// upload name each renamed local signature file maps to, plus a downloaded
/// copy of each present published signature asset. Any per-asset download
/// shortfall leaves the asset out of `downloaded` (its locally-produced
/// bytes are verified instead) with a verbose notice — never an issue.
fn published_signature_source(
    local: &std::collections::BTreeMap<String, (std::path::PathBuf, Option<String>, ArtifactKind)>,
    published: &[anodizer_stage_release::PublishedAsset],
    expected: &[String],
    is_signature: &dyn Fn(&str) -> bool,
    token: Option<&str>,
    download_dir: &std::path::Path,
    log: &StageLogger,
) -> anodizer_stage_sign::PublishedSignatureSource {
    let mut source = anodizer_stage_sign::PublishedSignatureSource::default();
    for (name, (path, _sha, kind)) in local {
        if matches!(kind, ArtifactKind::Signature | ArtifactKind::Certificate)
            && path.file_name().and_then(|n| n.to_str()) != Some(name.as_str())
        {
            source.uploaded_names.insert(path.clone(), name.clone());
        }
    }
    for (idx, name) in expected.iter().enumerate() {
        if !is_signature(name) {
            continue;
        }
        let Some(asset) = published.iter().find(|p| &p.name == name) else {
            continue;
        };
        if asset.size == 0 || asset.size > SIGNATURE_DOWNLOAD_CAP {
            continue;
        }
        // An index-derived filename, because the asset name is
        // remote-controlled data and must never influence the local path.
        let dest = download_dir.join(format!("published-{idx}"));
        match download_to_file(&asset.download_url, token, SIGNATURE_DOWNLOAD_CAP, &dest) {
            Ok(()) => {
                source.downloaded.insert(name.clone(), dest);
            }
            Err(e) => log.verbose(&format!(
                "could not download published signature asset '{name}' for \
                 cryptographic verification ({e:#}) — verifying the \
                 locally-produced bytes instead"
            )),
        }
    }
    source
}

/// Open an authenticated GET stream to a release asset, failing on any
/// non-success HTTP status.
fn asset_response(url: &str, token: Option<&str>) -> Result<reqwest::blocking::Response> {
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
    Ok(resp)
}

/// Download a release asset into `dest`, refusing to write more than `cap`
/// bytes.
fn download_to_file(
    url: &str,
    token: Option<&str>,
    cap: u64,
    dest: &std::path::Path,
) -> Result<()> {
    use std::io::Read as _;
    let mut reader = asset_response(url, token)?.take(cap + 1);
    let mut out = std::fs::File::create(dest)?;
    let copied = std::io::copy(&mut reader, &mut out)?;
    if copied > cap {
        anyhow::bail!("asset exceeds the {cap}-byte signature-download cap");
    }
    Ok(())
}

/// Download a release asset and return its sha256 hex, refusing to read more
/// than `cap` bytes — the digest fallback must never turn into an unbounded
/// re-download.
fn download_sha256(url: &str, token: Option<&str>, cap: u64) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let resp = asset_response(url, token)?;
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

/// Which check axes the selected publish surface leaves in scope, resolved
/// once in [`VerifyReleaseStage::run`] and read per crate.
struct AxisScope {
    /// github-release survives the operator selection (asset axis in scope).
    github_selected: bool,
    /// At least one OS-package carrier is selected (libc + smoke in scope).
    os_pkg_selected: bool,
    /// `install_smoke:` is configured.
    smoke_enabled: bool,
    /// Docker probe succeeded (only probed when smoke is in surface).
    docker_ok: bool,
}

/// Mutable cross-crate accumulation for the verify loop.
#[derive(Default)]
struct VerifyRun {
    /// Every post-publish defect found, across all crates and axes.
    issues: Vec<String>,
    /// The resolved install-smoke strategy is emitted once per run, on the
    /// first smoke job, so a CI operator can tell a slow copy path (dind
    /// without a shared work dir) from a fast bind-mount path.
    smoke_strategy_logged: bool,
}

/// Per-crate tally of what each check axis ACTUALLY examined.
///
/// An axis being enabled and in-surface proves only that it was in scope;
/// these counters prove it inspected ≥1 artifact — the difference between
/// "verified" and "had nothing to verify". The aggregation site refuses to
/// stamp a green verdict off all-zero counters, so a run that proved nothing
/// never fabricates passing evidence.
#[derive(Default)]
struct CrateVerifyOutcome {
    /// Published releases whose asset set was fetched and diffed/byte-checked.
    assets_inspected: usize,
    /// Packages whose embedded ELF was extracted and glibc-evaluated, or
    /// whose read failed (the failure pushed an issue, so it still counts as
    /// a real inspection).
    libc_inspected: usize,
    /// Packages actually submitted to the install-smoke matrix.
    smoke_inspected: usize,
}

impl CrateVerifyOutcome {
    /// Fold another crate's tally into this one.
    fn absorb(&mut self, other: &Self) {
        self.assets_inspected += other.assets_inspected;
        self.libc_inspected += other.libc_inspected;
        self.smoke_inspected += other.smoke_inspected;
    }

    /// Whether any axis examined at least one artifact.
    fn any_inspected(&self) -> bool {
        self.assets_inspected > 0 || self.libc_inspected > 0 || self.smoke_inspected > 0
    }
}

fn verify_one_crate(
    ctx: &Context,
    log: &StageLogger,
    rt: &tokio::runtime::Runtime,
    cfg: &VerifyReleaseConfig,
    crate_cfg: &CrateConfig,
    scope: &AxisScope,
    run: &mut VerifyRun,
) -> Result<CrateVerifyOutcome> {
    let mut outcome = CrateVerifyOutcome::default();
    // The caller filters to crates carrying a release block; if absent there
    // is no published release to verify, so skip this crate rather than panic.
    let Some(release_cfg) = crate_cfg.release.as_ref() else {
        return Ok(outcome);
    };
    let issues = &mut run.issues;

    // (a) asset existence + content ------------------------------------------
    if cfg.assert_assets_enabled() && scope.github_selected {
        match rt.block_on(anodizer_stage_release::fetch_published_assets(
            ctx,
            release_cfg,
            crate_cfg,
        )) {
            Ok(Some(published_assets)) => {
                outcome.assets_inspected += 1;
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
        && scope.os_pkg_selected
        && let Some(ceiling) = cfg.glibc_ceiling.as_deref()
    {
        for (path, name, _) in linux_packages(ctx, &crate_cfg.name) {
            if PackageType::from_filename(&name).is_none() {
                continue;
            }
            if check_one_package_libc(log, &crate_cfg.name, path, ceiling, issues) {
                outcome.libc_inspected += 1;
            }
        }
    }

    // (b) install smoke-test ------------------------------------------------
    // `smoke_enabled` is derived from `install_smoke.is_some()`; the `if let`
    // ties the config presence to its enablement flag without an unwrap.
    // Gating on the OS-package surface here (not only at the caller's
    // docker-probe site) keeps the loop's precondition local instead of
    // relying on the caller having zeroed `docker_ok` when out of surface.
    if scope.smoke_enabled
        && scope.os_pkg_selected
        && scope.docker_ok
        && let Some(smoke_cfg) = cfg.install_smoke.as_ref()
    {
        let binary = crate_binary_name(crate_cfg);
        for (path, name, target) in linux_packages(ctx, &crate_cfg.name) {
            let Some(pt) = PackageType::from_filename(&name) else {
                continue;
            };
            outcome.smoke_inspected += 1;
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
            if !run.smoke_strategy_logged {
                log.verbose(&format!(
                    "using install-smoke strategy {}",
                    smoke::strategy_label(&job.image)
                ));
                run.smoke_strategy_logged = true;
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

    Ok(outcome)
}

/// Run the libc-ceiling check on one Linux package's embedded ELF binary.
///
/// Returns whether an ELF was actually extracted and evaluated (or the read
/// failed, which pushed an issue) — `false` on the no-inspectable-ELF skip,
/// so the caller does not count a package that yielded nothing to check.
fn check_one_package_libc(
    log: &StageLogger,
    crate_name: &str,
    pkg_path: std::path::PathBuf,
    ceiling: &str,
    issues: &mut Vec<String>,
) -> bool {
    let elf_bytes = match extract_package_main_elf(&pkg_path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            log.verbose(&format!(
                "skipped libc check for crate '{crate_name}' {} — \
                 has no inspectable ELF",
                pkg_path.display()
            ));
            return false;
        }
        Err(e) => {
            issues.push(format!(
                "could not read {} of crate '{crate_name}' for the libc check: {e}",
                pkg_path.display()
            ));
            return true;
        }
    };
    match libc_check::check_glibc_ceiling(&elf_bytes, ceiling) {
        Ok(LibcCheckOutcome::NoGlibcRequirement) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} has no glibc requirement \
                 (static/musl) — skipped",
                pkg_path.display()
            ));
        }
        Ok(LibcCheckOutcome::WithinCeiling { max }) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} requires glibc {max} (<= {ceiling})",
                pkg_path.display()
            ));
        }
        Ok(LibcCheckOutcome::ExceedsCeiling { max, ceiling }) => {
            issues.push(format!(
                "{} of crate '{crate_name}' requires glibc {max}, exceeding the \
                 configured ceiling {ceiling}",
                pkg_path.display()
            ));
        }
        Err(e) => {
            issues.push(format!(
                "libc check failed for {} of crate '{crate_name}': {e}",
                pkg_path.display()
            ));
        }
    }
    true
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

/// Extract the largest executable ELF from a Linux package's payload —
/// `.deb` (ar + `data.tar.{gz,xz,zst}`), `.rpm` (lead + headers + compressed
/// cpio newc payload), or `.apk` (gzipped tar).
///
/// The shipped binary lives under `usr/bin/` (or similar). The largest
/// ELF member is picked as the binary to glibc-check — the common
/// single-binary case. Returns `Ok(None)` when no ELF is found (e.g. a
/// data-only package).
///
/// Extraction is intentionally minimal and dependency-free: it scans the
/// payload for ELF members by magic bytes. A malformed container or a
/// compression codec not linked into this build returns `Ok(None)` rather
/// than erroring — the libc check is best-effort.
fn extract_package_main_elf(pkg_path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let bytes = std::fs::read(pkg_path)?;
    let name = pkg_path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".deb") {
        let Some(data_tar) = deb::find_data_tar(&bytes)? else {
            return Ok(None);
        };
        Ok(deb::largest_elf_in_tar(&data_tar))
    } else if name.ends_with(".rpm") {
        rpm::payload_largest_elf(&bytes)
    } else if name.ends_with(".apk") {
        // An apk is (possibly concatenated) gzip streams of tar segments;
        // MultiGzDecoder crosses the stream boundaries so the data segment's
        // members are walked the same way the deb data.tar walk is.
        use std::io::Read as _;
        let mut tar_bytes = Vec::new();
        if flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut tar_bytes)
            .is_err()
        {
            return Ok(None);
        }
        Ok(deb::largest_elf_in_tar(&tar_bytes))
    } else {
        Ok(None)
    }
}

mod deb;
mod rpm;

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
