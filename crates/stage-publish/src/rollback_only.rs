//! `--rollback-only --from-run=<id>` mode.
//!
//! Loads `<dist>/run-<id>/report.json` (the `PublishReport` written at
//! end of a prior `PublishStage` run by the run-summary task) and
//! re-invokes each `Publisher`'s rollback for every `Succeeded` or
//! `RollbackFailed` entry. Writes the updated state to
//! `<dist>/run-<id>/rollback.json` so an operator can audit what was
//! attempted.
//!
//! No new publishing happens — the registry is loaded only to find the
//! matching `Publisher` impl per `result.name`. Submitter publishers and
//! entries that already terminated as `Failed`, `RolledBack`,
//! `Skipped(_)`, `PublishedNoRollback`, etc. are left untouched.
//!
//! The on-disk report at `<dist>/run-<id>/report.json` is the contract
//! between the run-summary task (writer) and this module (reader); the
//! file's format is `serde_json` of [`PublishReport`].

use anodizer_core::context::Context;
use anodizer_core::{PublishReport, Publisher, PublisherGroup, PublisherOutcome};
use anyhow::{Context as _, Result};
use std::fs;
use std::path::PathBuf;

/// Resolve the path to the prior run's `report.json` under
/// `<ctx.config.dist>/run-<id>/report.json`.
fn report_path(ctx: &Context, run_id: &str) -> PathBuf {
    ctx.config
        .dist
        .join(format!("run-{}", run_id))
        .join("report.json")
}

/// Resolve the path the replay writes its updated state to:
/// `<ctx.config.dist>/run-<id>/rollback.json`.
fn rollback_path(ctx: &Context, run_id: &str) -> PathBuf {
    ctx.config
        .dist
        .join(format!("run-{}", run_id))
        .join("rollback.json")
}

/// Load `<dist>/run-<id>/report.json` and re-attempt rollback for every
/// `Succeeded` or `RollbackFailed` Assets/Manager entry. Returns the
/// updated [`PublishReport`] and writes it to
/// `<dist>/run-<id>/rollback.json`.
///
/// Errors only when the prior report is missing or unparseable; per-step
/// rollback failures are recorded as `RollbackFailed` on the result and
/// do not abort the loop (mirrors the post-publish rollback runner).
pub fn run(ctx: &mut Context, run_id: &str) -> Result<PublishReport> {
    let publishers = crate::registry::configured_publishers(ctx);
    run_with_publishers(ctx, run_id, &publishers)
}

/// Test-injectable variant of [`run`]. Production callers use [`run`],
/// which constructs the publisher set from `ctx`. Tests pass a fake
/// registry directly so they can exercise the dispatch logic without
/// wiring a full publisher config.
pub(crate) fn run_with_publishers(
    ctx: &mut Context,
    run_id: &str,
    publishers: &[Box<dyn Publisher>],
) -> Result<PublishReport> {
    let log = ctx.logger("publish");

    let path = report_path(ctx, run_id);
    log.status(&format!(
        "rollback-only: loading prior run report from {}",
        path.display()
    ));

    let report_text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read prior report at {}", path.display()))?;
    let mut report: PublishReport = serde_json::from_str(&report_text)
        .with_context(|| format!("failed to parse prior report at {}", path.display()))?;

    // Re-attempt rollback for every Succeeded or RollbackFailed entry in
    // the Assets / Manager groups. Submitter publishers have no
    // programmatic rollback (warn-only) so they are skipped here too,
    // mirroring the live `rollback::run` policy.
    let target_indices: Vec<usize> = report
        .results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if !matches!(r.group, PublisherGroup::Assets | PublisherGroup::Manager) {
                return None;
            }
            match r.outcome {
                PublisherOutcome::Succeeded | PublisherOutcome::RollbackFailed(_) => Some(i),
                _ => None,
            }
        })
        .collect();

    if target_indices.is_empty() {
        log.warn(
            "rollback-only: no Succeeded or RollbackFailed entries in prior report; nothing to do",
        );
    } else {
        log.status(&format!(
            "rollback-only: dispatching {} target(s)",
            target_indices.len()
        ));
    }

    let mut rolled_back = 0usize;
    let mut failed = 0usize;
    let mut not_found = 0usize;

    for i in target_indices {
        let (name, evidence) = {
            let r = &report.results[i];
            (r.name.clone(), r.evidence.clone())
        };

        let Some(evidence) = evidence else {
            log.warn(&format!(
                "rollback-only: '{}' has no evidence in prior report; skipping",
                name,
            ));
            failed += 1;
            report.results[i].outcome =
                PublisherOutcome::RollbackFailed("no evidence in prior report".into());
            continue;
        };

        let Some(publisher) = publishers.iter().find(|p| p.name() == name) else {
            log.warn(&format!(
                "rollback-only: publisher '{}' not in current registry; skipping",
                name,
            ));
            not_found += 1;
            report.results[i].outcome =
                PublisherOutcome::RollbackFailed("publisher not found in current registry".into());
            continue;
        };

        log.status(&format!("rollback-only: invoking '{}'", name));
        match publisher.rollback(ctx, &evidence) {
            Ok(()) => {
                rolled_back += 1;
                report.results[i].outcome = PublisherOutcome::RolledBack;
            }
            Err(err) => {
                let msg = format!("{:#}", err);
                failed += 1;
                report.results[i].outcome = PublisherOutcome::RollbackFailed(msg.clone());
                log.warn(&format!("rollback-only: '{}' failed: {}", name, msg));
            }
        }
    }

    log.status(&format!(
        "rollback-only: {} rolled back, {} failed, {} not found",
        rolled_back, failed, not_found,
    ));

    // Persist the updated state to <dist>/run-<id>/rollback.json so the
    // operator has an audit trail of what was attempted on this replay.
    let out_path = rollback_path(ctx, run_id);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create rollback output dir {}", parent.display())
        })?;
    }
    let rollback_text =
        serde_json::to_string_pretty(&report).context("failed to serialize rollback report")?;
    fs::write(&out_path, rollback_text)
        .with_context(|| format!("failed to write rollback state to {}", out_path.display()))?;
    log.status(&format!("rollback-only: wrote {}", out_path.display()));

    Ok(report)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{
        PublishEvidence, Publisher, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };
    use tempfile::TempDir;

    /// Build a Context whose `config.dist` points at a fresh tempdir.
    /// Returns the context AND the TempDir guard so the directory
    /// outlives the test body.
    fn ctx_with_dist() -> (Context, TempDir) {
        let tmp = TempDir::new().expect("create tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist dir");
        let ctx = TestContextBuilder::new()
            .tag("v0.0.0-test")
            .dist(dist)
            .build();
        (ctx, tmp)
    }

    /// Helper to write a fixture report to `<dist>/run-<id>/report.json`.
    fn write_fixture_report(ctx: &Context, run_id: &str, report: &PublishReport) {
        let path = report_path(ctx, run_id);
        std::fs::create_dir_all(path.parent().unwrap()).expect("create run dir");
        let text = serde_json::to_string_pretty(report).expect("serialize report");
        std::fs::write(&path, text).expect("write fixture report");
    }

    fn succeeded_entry(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(PublishEvidence::new(name)),
        }
    }

    #[test]
    fn rollback_only_reads_report_and_dispatches() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "succeeded entry should flip to RolledBack, got {:?}",
            updated.results[0].outcome,
        );
        let out = rollback_path(&ctx, "fixt");
        assert!(out.exists(), "rollback.json must be written");
    }

    #[test]
    fn rollback_only_marks_publisher_not_found_when_registry_lacks_it() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("orphan", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Empty registry — the report names a publisher we no longer have.
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        match &updated.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(
                    msg.contains("not found in current registry"),
                    "expected not-found message, got '{}'",
                    msg,
                );
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_only_bails_when_report_path_missing() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let err = run_with_publishers(&mut ctx, "nonexistent", &publishers)
            .expect_err("must error when prior report missing");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("failed to read prior report"),
            "error must reference missing report path, got '{}'",
            msg,
        );
    }

    #[test]
    fn rollback_only_skips_non_succeeded_entries() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        // One Failed entry (run() never succeeded; nothing to roll back).
        report.results.push(PublisherResult {
            name: "failed-mgr".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::Failed("boom".into()),
            evidence: None,
        });
        // One Skipped entry (e.g. submitter gated).
        report.results.push(PublisherResult {
            name: "skipped-sub".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Skipped(SkipReason::SubmitterGated),
            evidence: None,
        });
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![
            fake(
                "failed-mgr",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Succeed,
            ),
            fake(
                "skipped-sub",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        // Nothing changed: Failed stays Failed; Skipped stays Skipped.
        match &updated.results[0].outcome {
            PublisherOutcome::Failed(msg) => assert!(msg.contains("boom")),
            other => panic!("expected Failed unchanged, got {:?}", other),
        }
        assert!(matches!(
            updated.results[1].outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));

        // rollback.json still written so the operator has an artifact.
        assert!(rollback_path(&ctx, "fixt").exists());
    }

    #[test]
    fn rollback_only_writes_rollback_json() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        let out = rollback_path(&ctx, "fixt");
        let text = std::fs::read_to_string(&out).expect("read rollback.json");
        let parsed: PublishReport = serde_json::from_str(&text).expect("parse rollback.json");
        assert!(matches!(
            parsed.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
    }

    #[test]
    fn rollback_only_retries_rollback_failed_entries() {
        // RollbackFailed entries from a prior run should be re-attempted —
        // that's the whole point of having the operator re-invoke
        // --rollback-only after fixing whatever blocked the live rollback.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "mgr1".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::RollbackFailed("transient failure".into()),
            evidence: Some(PublishEvidence::new("mgr1")),
        });
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "RollbackFailed should re-attempt and flip to RolledBack on success, got {:?}",
            updated.results[0].outcome,
        );
    }

    #[test]
    fn rollback_only_records_failure_per_step() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake_with_rollback(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("rollback bang".into()),
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        match &updated.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => assert!(msg.contains("rollback bang")),
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_only_bails_when_report_unparseable() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let path = report_path(&ctx, "fixt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-json").unwrap();

        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let err = run_with_publishers(&mut ctx, "fixt", &publishers)
            .expect_err("must error on unparseable report");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("failed to parse prior report"),
            "error must reference parse failure, got '{}'",
            msg,
        );
    }
}
