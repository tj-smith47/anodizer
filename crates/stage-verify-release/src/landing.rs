//! Per-publisher post-publish landing checks.
//!
//! A publisher reporting `Succeeded` proves its client call returned OK — not
//! that consumers can actually SEE the published artifact. This module closes
//! that gap for the publishers whose landed surface is independently
//! probeable with coordinates the run already recorded in its own publish
//! report:
//!
//! - **cargo** — every published crate version must be visible on the
//!   crates.io sparse index (custom registries are skipped: the crates.io
//!   index says nothing about them).
//! - **npm** — every published package version must answer a registry
//!   metadata `GET`.
//! - **blob** — every uploaded object must answer a `HEAD` through the same
//!   `ObjectStore` backend (and ambient credential chain) the upload used —
//!   buckets rarely expose a public read URL, so this is the strongest
//!   honest probe available.
//!
//! Only publishers whose recorded outcome is `Succeeded` are PROBED: a
//! skipped / deselected / rolled-back publisher landed nothing this run, so
//! there is nothing to verify (and probing it would report defects the
//! publish never claimed to avoid). A probe that cannot run — network
//! failure, store build failure — is itself reported as an issue: this
//! stage's whole job is verification, and an unverifiable landing is a
//! finding, not a pass.
//!
//! A publisher that was ATTEMPTED and reported `Failed` is a different case:
//! the run tried to ship it and did not. That is a landing defect on its own
//! merits — recorded without a network probe. How a landing finding (a failed
//! publish attempt, or a probe that could not confirm the upload landed) is
//! reported follows the publisher's `required` flag: a REQUIRED publisher's
//! landing finding is a gate-failing issue; an advisory (`required: false`)
//! publisher's is a loud, recorded WARNING that never fails the release.
//! Non-silent — the failure still shows in this log line and in the publish
//! report's own `Failed` row that the run summary renders — while honouring
//! the operator's explicit tolerance. This stops an optional publisher from
//! stranding the required ones: in the release workflow a failed
//! verify-release skips the downstream OIDC leg, so a fatal optional landing
//! finding would block crates.io / npm / PyPI.
//!
//! This `required`-routing governs PUBLISHER landing findings only. The
//! artifact-quality / integrity gates the operator opts into elsewhere in
//! verify-release (asset-existence, install-smoke, glibc-ceiling, signature
//! crypto-verification) are NOT publisher-tolerance questions — a broken,
//! missing, or forged artifact is a release defect regardless of which
//! publisher shipped it — so they stay fatal by the operator's own opt-in.
//!
//! The probes are injected as closures so the orchestration (report
//! filtering, evidence decoding, issue wording) is unit-testable offline;
//! `VerifyReleaseStage::run` supplies the real network-backed
//! implementations.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::publish_evidence::{
    BlobTargetSnapshot, CargoYankTargetSnapshot, NpmTargetSnapshot, PublishEvidenceExtra,
    SnapcraftTargetSnapshot,
};
use anodizer_core::publish_report::{PublisherOutcome, PublisherResult};

/// `(snap, version, channel)` probe signature for the Snap Store channel-map
/// check (see [`LandingProbes::snap_channel_map`]).
pub type SnapChannelMapProbe<'a> = dyn Fn(&str, &str, Option<&str>) -> anyhow::Result<bool> + 'a;

/// The landing probes, injected so tests can drive the orchestration without
/// a network.
pub struct LandingProbes<'a> {
    /// `(crate_name, version)` → whether the version is visible on the
    /// crates.io sparse index. `Err` = the index could not be consulted.
    pub cargo_index: &'a dyn Fn(&str, &str) -> anyhow::Result<bool>,
    /// `(registry, package, version)` → whether the version answers a
    /// registry metadata GET. `Ok(false)` = a definitive 404, `Err` = the
    /// registry could not be consulted (5xx/transport) — an npm version is
    /// immutable, so an outage must not be reported as "not visible".
    pub npm_registry: &'a dyn Fn(&str, &str, &str) -> anyhow::Result<bool>,
    /// Blob target → whether the object exists in its bucket. `Err` = the
    /// store could not be built or the HEAD failed indeterminately.
    pub blob_head: &'a dyn Fn(&BlobTargetSnapshot) -> anyhow::Result<bool>,
    /// `(snap, version, channel)` → whether the version is live in the Snap
    /// Store's channel map (in the given channel, or any channel when
    /// `None`). `Ok(false)` covers snap-unknown and version-absent alike;
    /// `Err` = the store could not be consulted.
    pub snap_channel_map: &'a SnapChannelMapProbe<'a>,
}

/// Run every applicable landing check against the run's publish report.
///
/// Returns the number of publishers actually probed, so the caller can tell
/// "everything verified" apart from "nothing was in scope to verify" when
/// deciding whether to stamp a verdict.
pub(crate) fn run_landing_checks(
    ctx: &Context,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> usize {
    let Some(report) = ctx.publish_report() else {
        log.verbose("no publish report recorded this run — landing checks skipped");
        return 0;
    };
    let mut probed_publishers = 0usize;
    for result in &report.results {
        // A publisher's landing findings are routed by its `required` flag: a
        // required publisher's finding fails the gate; an advisory
        // (`required: false`) publisher's is a loud, recorded warning that
        // never fails the release — so an optional publisher can neither
        // silently pass nor, by failing, block the required ones (a fatal
        // verify-release skips the downstream OIDC leg, stranding crates.io /
        // npm / PyPI). Operator-enabled artifact-quality gates elsewhere in
        // this stage stay fatal regardless; see the module doc.
        let mut findings: Vec<String> = Vec::new();
        let probed = if let PublisherOutcome::Succeeded = result.outcome {
            match result.name.as_str() {
                "cargo" => check_cargo_landing(result, log, probes, &mut findings),
                "npm" => check_npm_landing(result, log, probes, &mut findings),
                "blob" => check_blob_landing(result, log, probes, &mut findings),
                "snapcraft" => check_snapcraft_landing(result, log, probes, &mut findings),
                _ => false,
            }
        } else if let PublisherOutcome::Failed(reason) = &result.outcome {
            // A publisher the run actually attempted and failed is a landing
            // defect on its own merits (no probe needed) — every publisher,
            // not just the four network-probed ones.
            log.warn(&format!(
                "{} publish attempt failed this run — landing not verified: {reason}",
                result.name
            ));
            findings.push(format!(
                "{}: publish attempt failed this run — {reason}",
                result.name
            ));
            false
        } else {
            log.verbose(&format!(
                "skipped {} landing check — publisher did not succeed this run ({:?})",
                result.name, result.outcome
            ));
            false
        };
        if result.required {
            issues.extend(findings);
        } else {
            for finding in findings {
                log.warn(&format!(
                    "optional publisher not gating the release (required: false) — {finding}"
                ));
            }
        }
        if probed {
            probed_publishers += 1;
        }
    }
    probed_publishers
}

/// Decode the cargo publisher's recorded publish targets.
fn cargo_targets(result: &PublisherResult) -> &[CargoYankTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Cargo(extra)) => &extra.cargo_yank_targets,
        _ => &[],
    }
}

/// Decode the npm publisher's recorded publish targets.
fn npm_targets(result: &PublisherResult) -> &[NpmTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Npm(extra)) => &extra.npm_targets,
        _ => &[],
    }
}

/// Decode the blob publisher's recorded upload targets.
fn blob_targets(result: &PublisherResult) -> &[BlobTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Blob(extra)) => &extra.blob_targets,
        _ => &[],
    }
}

/// Probe every crates.io-targeted crate the cargo publisher recorded.
/// Returns whether at least one target was probed.
fn check_cargo_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = cargo_targets(result);
    if targets.is_empty() {
        log.verbose("cargo succeeded but recorded no published crates — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut probed = 0usize;
    for t in targets {
        // A custom registry/index means the crates.io sparse index is not
        // authoritative for this target — the same scoping the publisher's
        // own idempotency guard applies.
        if t.registry.is_some() || t.index.is_some() {
            log.verbose(&format!(
                "skipped index probe for {}@{} — published to a non-crates.io registry",
                t.name, t.version
            ));
            continue;
        }
        probed += 1;
        match (probes.cargo_index)(&t.name, &t.version) {
            Ok(true) => visible.push(format!("{}@{}", t.name, t.version)),
            Ok(false) => issues.push(format!(
                "cargo: {}@{} reported published but is not visible on the \
                 crates.io index",
                t.name, t.version
            )),
            Err(e) => issues.push(format!(
                "cargo: could not probe the crates.io index for {}@{}: {e:#}",
                t.name, t.version
            )),
        }
    }
    if probed > 0 && visible.len() == probed {
        if probed == 1 {
            log.status(&format!("cargo: {} visible on crates.io index", visible[0]));
        } else {
            log.status(&format!(
                "cargo: {probed}/{probed} published crate(s) visible on crates.io index"
            ));
        }
    }
    probed > 0
}

/// Trim the URL scheme off a registry endpoint for concise status wording.
fn registry_host(registry: &str) -> &str {
    registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// Probe every package version the npm publisher recorded.
/// Returns whether at least one target was probed.
fn check_npm_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = npm_targets(result);
    if targets.is_empty() {
        log.verbose("npm succeeded but recorded no published packages — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    for t in targets {
        match (probes.npm_registry)(&t.registry, &t.package, &t.version) {
            Ok(true) => visible.push(format!("{}@{}", t.package, t.version)),
            Ok(false) => issues.push(format!(
                "npm: {}@{} reported published but is not visible on {}",
                t.package,
                t.version,
                registry_host(&t.registry)
            )),
            // Indeterminate: the registry could not be consulted. An npm
            // version is immutable once published, so a transient outage must
            // fail closed as "unverifiable", never as "not visible" — the
            // latter would fail an already-landed one-way-door release.
            Err(e) => issues.push(format!(
                "npm: could not confirm {}@{} on {}: {e:#}",
                t.package,
                t.version,
                registry_host(&t.registry)
            )),
        }
    }
    if visible.len() == targets.len() {
        let host = registry_host(&targets[0].registry);
        if targets.len() == 1 {
            log.status(&format!("npm: {} visible on {host}", visible[0]));
        } else {
            log.status(&format!(
                "npm: {0}/{0} published package(s) visible on {host}",
                targets.len()
            ));
        }
    }
    true
}

/// HEAD every object the blob publisher recorded.
/// Returns whether at least one target was probed.
fn check_blob_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = blob_targets(result);
    if targets.is_empty() {
        log.verbose("blob succeeded but recorded no uploaded objects — nothing to probe");
        return false;
    }
    let mut present = 0usize;
    for t in targets {
        let url = format!("{}://{}/{}", t.provider, t.bucket, t.key);
        match (probes.blob_head)(t) {
            Ok(true) => {
                present += 1;
                log.verbose(&format!("{url} present"));
            }
            Ok(false) => issues.push(format!(
                "blob: {url} reported uploaded but is missing from the bucket"
            )),
            Err(e) => issues.push(format!("blob: could not verify {url}: {e:#}")),
        }
    }
    if present == targets.len() {
        log.status(&format!(
            "blob: {present}/{} uploaded object(s) present in bucket",
            targets.len()
        ));
    }
    true
}

/// Decode the snapcraft publisher's recorded upload targets.
fn snapcraft_targets(result: &PublisherResult) -> &[SnapcraftTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Snapcraft(extra)) => &extra.snapcraft_targets,
        _ => &[],
    }
}

/// Probe the Snap Store channel map for every snap the snapcraft publisher
/// recorded. Returns whether at least one target was probed.
///
/// A `snapcraft upload` OK proves acceptance, not delivery: a manual-review
/// hold parks the revision outside every channel until a human approves it,
/// and a decline arrives only by email — so an absent version is reported as
/// an issue either way, with the hold context in the wording when the run
/// recorded one. A held snap that review has since approved probes visible
/// and passes cleanly.
fn check_snapcraft_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = snapcraft_targets(result);
    if targets.is_empty() {
        log.verbose("snapcraft succeeded but recorded no uploaded snaps — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut probed = 0usize;
    for t in targets {
        // Pre-`version` snapshots (older runs replayed via --from-run) carry
        // no version to look for — nothing honest to probe.
        let Some(version) = t.version.as_deref() else {
            log.verbose(&format!(
                "skipped store probe for snap '{}' — no version recorded in the run snapshot",
                t.package_name
            ));
            continue;
        };
        probed += 1;
        let coords = format!("{} {version}", t.package_name);
        match (probes.snap_channel_map)(&t.package_name, version, t.channel.as_deref()) {
            Ok(true) => visible.push(coords),
            Ok(false) if t.held_for_review => issues.push(format!(
                "snapcraft: {coords} was HELD for Snap Store manual review and is not live in \
                 the store — consumers get nothing until review approves \
                 (https://dashboard.snapcraft.io/snaps/{}/)",
                t.package_name
            )),
            Ok(false) => issues.push(format!(
                "snapcraft: {coords} reported uploaded but is not in the store's channel map{}",
                t.channel
                    .as_deref()
                    .map(|c| format!(" for channel '{c}'"))
                    .unwrap_or_default()
            )),
            Err(e) => issues.push(format!(
                "snapcraft: could not probe the Snap Store for {coords}: {e:#}"
            )),
        }
    }
    if probed > 0 && visible.len() == probed {
        if probed == 1 {
            log.status(&format!(
                "snapcraft: {} live in the Snap Store channel map",
                visible[0]
            ));
        } else {
            log.status(&format!(
                "snapcraft: {probed}/{probed} uploaded snap(s) live in the Snap Store channel map"
            ));
        }
    }
    probed > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::publish_evidence::{BlobExtra, CargoExtra, NpmExtra, PublishEvidence};
    use anodizer_core::publish_report::{PublishReport, PublisherGroup, SkipReason};
    use std::cell::Cell;

    fn ctx_with_report(report: PublishReport) -> Context {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.set_publish_report(report);
        ctx
    }

    fn result_with(
        name: &str,
        outcome: PublisherOutcome,
        extra: PublishEvidenceExtra,
    ) -> PublisherResult {
        let mut evidence = PublishEvidence::new(name);
        evidence.extra = extra;
        PublisherResult {
            name: name.to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome,
            evidence: Some(evidence),
        }
    }

    fn cargo_extra(targets: &[(&str, &str)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Cargo(CargoExtra {
            cargo_yank_targets: targets
                .iter()
                .map(|(n, v)| CargoYankTargetSnapshot {
                    name: n.to_string(),
                    version: v.to_string(),
                    registry: None,
                    index: None,
                })
                .collect(),
        })
    }

    fn npm_extra(targets: &[(&str, &str)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Npm(NpmExtra {
            npm_targets: targets
                .iter()
                .map(|(p, v)| NpmTargetSnapshot {
                    target: p.to_string(),
                    package: p.to_string(),
                    version: v.to_string(),
                    registry: "https://registry.npmjs.org".to_string(),
                    dist_tag: "latest".to_string(),
                    ..Default::default()
                })
                .collect(),
        })
    }

    fn blob_extra(keys: &[&str]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Blob(BlobExtra {
            blob_targets: keys
                .iter()
                .map(|k| BlobTargetSnapshot {
                    provider: "s3".to_string(),
                    bucket: "bkt".to_string(),
                    key: k.to_string(),
                    region: None,
                    endpoint: None,
                })
                .collect(),
        })
    }

    /// Probes that must never fire — for paths that filter before probing.
    fn panicking_probes() -> LandingProbes<'static> {
        LandingProbes {
            cargo_index: &|n, v| panic!("cargo probe must not fire for {n}@{v}"),
            npm_registry: &|_, p, v| panic!("npm probe must not fire for {p}@{v}"),
            blob_head: &|t| panic!("blob probe must not fire for {}", t.key),
            snap_channel_map: &|s, v, _| panic!("snap probe must not fire for {s} {v}"),
        }
    }

    fn test_logger(ctx: &Context) -> StageLogger {
        ctx.logger("verify-release")
    }

    #[test]
    fn no_publish_report_probes_nothing() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn skipped_and_rolled_back_publishers_are_never_probed_or_flagged() {
        // Genuine skips / an intentionally-reverted publish landed nothing
        // this run — no probe, no issue.
        let report = PublishReport {
            results: vec![
                result_with(
                    "npm",
                    PublisherOutcome::Skipped(SkipReason::Deselected),
                    npm_extra(&[("app", "1.0.0")]),
                ),
                result_with("blob", PublisherOutcome::RolledBack, blob_extra(&["k"])),
            ],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn attempted_and_failed_publisher_is_reported_as_a_landing_issue() {
        // A publisher the run actually tried to ship and failed is a landing
        // defect on its own — no probe needed, and it must not be silently
        // swallowed like a genuine skip.
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Failed("boom".into()),
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("cargo"));
        assert!(issues[0].contains("boom"));
    }

    #[test]
    fn failed_snapcraft_publish_is_reported_as_a_landing_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Failed("store rejected upload: dedup collision".into()),
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("snapcraft"));
        assert!(issues[0].contains("dedup collision"));
    }

    /// Flip a result to advisory (`required: false`).
    fn optional(mut result: PublisherResult) -> PublisherResult {
        result.required = false;
        result
    }

    #[test]
    fn optional_publisher_failure_warns_but_never_fails_the_gate() {
        // The v0.21.0 regression: snapcraft is advisory (required: false) and
        // failed a Snap Store content-dedup. verify-release must NOT turn that
        // into a gate-failing issue — a fatal verdict here skips the downstream
        // OIDC leg and strands the REQUIRED registries (crates.io / npm / PyPI).
        let report = PublishReport {
            results: vec![optional(result_with(
                "snapcraft",
                PublisherOutcome::Failed(
                    "store rejected upload: content-identical dedup at another version".into(),
                ),
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            ))],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(
            issues.is_empty(),
            "an optional publisher's failure must never fail the gate: {issues:?}"
        );
    }

    #[test]
    fn required_publisher_failure_still_fails_the_gate() {
        // The `required` knob is the single source of truth: a required
        // publisher's failure stays fatal even after optional ones are relaxed.
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Failed("registry 500".into()),
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(issues.len(), 1, "required failure must remain fatal");
        assert!(issues[0].contains("cargo"));
    }

    #[test]
    fn optional_publisher_unverifiable_landing_warns_but_never_fails_the_gate() {
        // A required: false publisher that reported success but whose landing
        // cannot be confirmed (probe says absent) is a false-success — still
        // surfaced (warned) but never gate-failing, since the operator marked
        // it advisory. Required publishers keep failing on the same condition.
        let report = PublishReport {
            results: vec![optional(result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            ))],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let probes = LandingProbes {
            cargo_index: &|n, v| panic!("cargo probe must not fire for {n}@{v}"),
            npm_registry: &|_, p, v| panic!("npm probe must not fire for {p}@{v}"),
            blob_head: &|t| panic!("blob probe must not fire for {}", t.key),
            snap_channel_map: &|_, _, _| Ok(false),
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            issues.is_empty(),
            "an optional publisher's unverifiable landing must not fail the gate: {issues:?}"
        );
    }

    #[test]
    fn attempted_and_failed_homebrew_publisher_is_reported_as_a_landing_issue() {
        // homebrew has no independently-probeable landing surface (no cargo
        // index / npm registry / bucket / Snap Store equivalent), but a
        // publisher this run actually attempted and failed is a landing
        // defect on its own merits regardless of whether it's one of the
        // four network-probed publishers — the name-list must not gate
        // whether a failure gets surfaced.
        let report = PublishReport {
            results: vec![result_with(
                "homebrew",
                PublisherOutcome::Failed("tap push rejected: formula conflict".into()),
                PublishEvidenceExtra::Empty,
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(
            probed, 0,
            "homebrew has no landing probe, only the issue report"
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("homebrew"));
        assert!(issues[0].contains("formula conflict"));
    }

    #[test]
    fn config_skipped_snapcraft_is_not_a_landing_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Skipped(SkipReason::NotConfigured),
                snapcraft_extra(&[]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn cargo_visible_versions_produce_no_issues() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0"), ("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let calls = Cell::new(0usize);
        let cargo = |_: &str, _: &str| -> anyhow::Result<bool> {
            calls.set(calls.get() + 1);
            Ok(true)
        };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1, "one publisher probed");
        assert_eq!(calls.get(), 2, "every published crate probed");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn cargo_missing_version_is_an_issue_naming_the_crate() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0"), ("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let cargo = |name: &str, _: &str| -> anyhow::Result<bool> { Ok(name != "app-core") };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("app-core@1.0.0") && issues[0].contains("not visible"),
            "{issues:?}"
        );
    }

    #[test]
    fn cargo_probe_error_is_an_issue_not_a_silent_pass() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let cargo =
            |_: &str, _: &str| -> anyhow::Result<bool> { anyhow::bail!("index unreachable") };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not probe") && issues[0].contains("index unreachable"),
            "{issues:?}"
        );
    }

    #[test]
    fn cargo_custom_registry_targets_are_skipped_not_probed() {
        let extra = PublishEvidenceExtra::Cargo(CargoExtra {
            cargo_yank_targets: vec![CargoYankTargetSnapshot {
                name: "app".to_string(),
                version: "1.0.0".to_string(),
                registry: Some("my-registry".to_string()),
                index: None,
            }],
        });
        let report = PublishReport {
            results: vec![result_with("cargo", PublisherOutcome::Succeeded, extra)],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        // Panicking cargo probe proves the custom-registry target never
        // reaches the crates.io index probe.
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0, "no crates.io-scoped target => nothing probed");
        assert!(issues.is_empty());
    }

    #[test]
    fn npm_invisible_version_is_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("@scope/app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let npm = |_: &str, _: &str, _: &str| Ok(false);
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("@scope/app@1.0.0")
                && issues[0].contains("registry.npmjs.org")
                && issues[0].contains("is not visible"),
            "a definitive 404 must read as 'not visible': {issues:?}"
        );
    }

    #[test]
    fn npm_indeterminate_probe_is_a_distinct_issue_not_not_visible() {
        // A registry that could not be consulted (5xx/transport) must fail
        // closed as "could not confirm", never as "not visible" — an npm
        // version is immutable, so the latter would fail an already-landed
        // release on a transient outage.
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("app", "3.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let npm =
            |_: &str, _: &str, _: &str| Err(anyhow::anyhow!("502 Bad Gateway: registry down"));
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not confirm app@3.0.0")
                && !issues[0].contains("is not visible"),
            "indeterminate must be its own issue, not a false 'not visible': {issues:?}"
        );
    }

    #[test]
    fn npm_visible_version_passes_with_recorded_coordinates() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("app", "2.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let seen = Cell::new(false);
        let npm = |registry: &str, package: &str, version: &str| {
            assert_eq!(registry, "https://registry.npmjs.org");
            assert_eq!(package, "app");
            assert_eq!(version, "2.0.0");
            seen.set(true);
            Ok(true)
        };
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(seen.get(), "probe must receive the recorded coordinates");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn blob_missing_and_unverifiable_objects_are_distinct_issues() {
        let report = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["v1/app.tar.gz", "v1/checksums.txt", "v1/app.sig"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let blob = |t: &BlobTargetSnapshot| -> anyhow::Result<bool> {
            match t.key.as_str() {
                "v1/app.tar.gz" => Ok(true),
                "v1/checksums.txt" => Ok(false),
                _ => anyhow::bail!("HEAD timed out"),
            }
        };
        let probes = LandingProbes {
            blob_head: &blob,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 2);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("s3://bkt/v1/checksums.txt") && i.contains("missing")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("s3://bkt/v1/app.sig") && i.contains("could not verify")),
            "{issues:?}"
        );
    }

    fn snapcraft_extra(targets: &[(&str, &str, Option<&str>, bool)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Snapcraft(anodizer_core::publish_evidence::SnapcraftExtra {
            snapcraft_targets: targets
                .iter()
                .map(|(name, version, channel, held)| SnapcraftTargetSnapshot {
                    crate_name: name.to_string(),
                    package_name: name.to_string(),
                    channel: channel.map(|c| c.to_string()),
                    revision: None,
                    version: Some(version.to_string()),
                    held_for_review: *held,
                })
                .collect(),
        })
    }

    #[test]
    fn snapcraft_visible_version_passes_with_recorded_coordinates() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let seen = Cell::new(false);
        let snap = |name: &str, version: &str, channel: Option<&str>| {
            assert_eq!(name, "demo");
            assert_eq!(version, "1.2.3");
            assert_eq!(channel, Some("stable"));
            seen.set(true);
            Ok(true)
        };
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert!(seen.get(), "probe must receive the recorded coordinates");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn snapcraft_held_and_absent_version_reports_the_review_hold() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), true)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| Ok(false);
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("demo 1.2.3")
                && issues[0].contains("HELD for Snap Store manual review")
                && issues[0].contains("dashboard.snapcraft.io/snaps/demo/"),
            "a held-and-absent snap must name the review hold: {issues:?}"
        );
    }

    #[test]
    fn snapcraft_absent_version_without_hold_reads_as_not_in_channel_map() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "2.0.0", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| Ok(false);
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("not in the store's channel map for channel 'stable'")
                && !issues[0].contains("HELD"),
            "{issues:?}"
        );
    }

    #[test]
    fn snapcraft_probe_error_is_an_issue_not_a_silent_pass() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.0.0", None, false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| anyhow::bail!("store unreachable");
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not probe") && issues[0].contains("store unreachable"),
            "{issues:?}"
        );
    }

    #[test]
    fn snapcraft_versionless_snapshot_is_skipped_not_probed() {
        // A pre-`version` snapshot replayed via --from-run has nothing honest
        // to probe — the panicking probe proves it is never consulted.
        let extra =
            PublishEvidenceExtra::Snapcraft(anodizer_core::publish_evidence::SnapcraftExtra {
                snapcraft_targets: vec![SnapcraftTargetSnapshot {
                    crate_name: "demo".to_string(),
                    package_name: "demo".to_string(),
                    channel: Some("stable".to_string()),
                    revision: None,
                    version: None,
                    held_for_review: false,
                }],
            });
        let report = PublishReport {
            results: vec![result_with("snapcraft", PublisherOutcome::Succeeded, extra)],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0, "no version recorded => nothing probed");
        assert!(issues.is_empty());
    }

    #[test]
    fn unrelated_succeeded_publishers_are_ignored() {
        let report = PublishReport {
            results: vec![result_with(
                "homebrew",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Empty,
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }
}
