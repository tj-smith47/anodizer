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
//! Only publishers whose recorded outcome is `Succeeded` are probed: a
//! skipped / deselected / failed publisher landed nothing this run, so there
//! is nothing to verify (and probing it would report defects the publish
//! never claimed to avoid). A probe that cannot run — network failure, store
//! build failure — is itself reported as an issue: this stage's whole job is
//! verification, and an unverifiable landing is a finding, not a pass.
//!
//! The probes are injected as closures so the orchestration (report
//! filtering, evidence decoding, issue wording) is unit-testable offline;
//! `VerifyReleaseStage::run` supplies the real network-backed
//! implementations.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::publish_evidence::{
    BlobTargetSnapshot, CargoYankTargetSnapshot, NpmTargetSnapshot, PublishEvidenceExtra,
};
use anodizer_core::publish_report::{PublisherOutcome, PublisherResult};

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
        if !matches!(result.outcome, PublisherOutcome::Succeeded) {
            if matches!(result.name.as_str(), "cargo" | "npm" | "blob") {
                log.verbose(&format!(
                    "skipped {} landing check — publisher did not succeed this run ({:?})",
                    result.name, result.outcome
                ));
            }
            continue;
        }
        let probed = match result.name.as_str() {
            "cargo" => check_cargo_landing(result, log, probes, issues),
            "npm" => check_npm_landing(result, log, probes, issues),
            "blob" => check_blob_landing(result, log, probes, issues),
            _ => false,
        };
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
    fn non_succeeded_publishers_are_never_probed() {
        // Failed / skipped / rolled-back publishers landed nothing — no probe.
        let report = PublishReport {
            results: vec![
                result_with(
                    "cargo",
                    PublisherOutcome::Failed("boom".into()),
                    cargo_extra(&[("app", "1.0.0")]),
                ),
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
