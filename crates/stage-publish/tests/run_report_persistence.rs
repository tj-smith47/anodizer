//! Integration: end-of-PublishStage writes `<dist>/run-<id>/report.json`,
//! and `rollback_only::run` reads it back without a "report not found"
//! error.
//!
//! Closes the writer/reader gap surfaced by the 2026-05-15 release-
//! resilience audit (finding C1): before B4, no production code wrote
//! `report.json`, so `--rollback-only --from-run=<id>` was structurally
//! unreachable. This test pins the round-trip via the production
//! writer (`write_report_to_run_dir`, doc-hidden + pub for the same
//! test-injection reason as `run_with_publishers`) and the production
//! reader (`rollback_only::run`).

use anodizer_core::context::Context;
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PublishEvidence, Publisher, PublisherGroup, PublisherOutcome};
use anodizer_stage_publish::{PublishStage, rollback_only, write_report_to_run_dir};

/// Minimal in-test Publisher with a no-op rollback. Mirrors
/// `tests/sibling_isolation.rs` (which avoids the crate-internal
/// `testing` module since it's `pub(crate)`).
struct SuccessPublisher {
    name: &'static str,
    group: PublisherGroup,
}

impl Publisher for SuccessPublisher {
    fn name(&self) -> &str {
        self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        false
    }
    fn skips_on_nightly(&self) -> bool {
        false
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        Ok(PublishEvidence::new(self.name))
    }
    fn rollback(&self, _ctx: &mut Context, _ev: &PublishEvidence) -> anyhow::Result<()> {
        // No-op rollback; this test asserts the read-back path, not
        // the dispatch logic (covered by rollback_only unit tests).
        Ok(())
    }
}

#[test]
fn publish_stage_writes_report_and_rollback_only_can_read_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut ctx = TestContextBuilder::new()
        .tag("v0.0.0-test")
        .dist(tmp.path().to_path_buf())
        .build();

    // Synthetic publishers — `PublishStage::run` reads the registry
    // from config, which would skip our fakes, so we drive the
    // doc-hidden `run_with_publishers` entry point that production
    // also uses (it's the seam every dispatcher test in the crate
    // takes). Then we call the doc-hidden writer the same way
    // `PublishStage::run` does, end-of-pipeline.
    let publishers: Vec<Box<dyn Publisher>> = vec![
        Box::new(SuccessPublisher {
            name: "manager-only",
            group: PublisherGroup::Manager,
        }),
        Box::new(SuccessPublisher {
            name: "assets-only",
            group: PublisherGroup::Assets,
        }),
    ];

    let log = ctx.logger("publish-test-integration");
    PublishStage::run_with_publishers(&mut ctx, &log, &publishers).expect("run_with_publishers Ok");

    // Production writer — exact call PublishStage::run makes after
    // dispatch + rollback. With both fakes succeeding, no rollback
    // fires (only required-failure path invokes rollback dispatch),
    // so this captures the steady-state "all succeeded" report.
    write_report_to_run_dir(&ctx, &log);

    let path = tmp.path().join("run-v0.0.0-test").join("report.json");
    assert!(
        path.exists(),
        "PublishStage writer must persist report at {}",
        path.display()
    );

    // Round-trip: parse back via PublishReport's serde shape; assert
    // both publisher entries survived.
    let body = std::fs::read_to_string(&path).expect("read");
    let parsed: anodizer_core::PublishReport =
        serde_json::from_str(&body).expect("PublishReport round-trip");
    assert_eq!(parsed.results.len(), 2, "round-trip preserves all entries");
    let mut names: Vec<&str> = parsed.results.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["assets-only", "manager-only"]);
    for r in &parsed.results {
        assert!(
            matches!(r.outcome, PublisherOutcome::Succeeded),
            "expected Succeeded for {}, got {:?}",
            r.name,
            r.outcome,
        );
    }

    // Reader: rollback_only::run reads the same path and must not
    // error with "failed to read prior report". The synthetic
    // publishers aren't wired into the config-driven registry (which
    // is what `rollback_only::run` consults), so each entry flips to
    // `RollbackFailed("publisher not found in current registry")` —
    // that's a faithful reflection of production behavior when an
    // operator runs `--rollback-only` against a run that targeted
    // publishers since removed from the config.
    //
    // What matters for THIS test (the writer/reader contract): the
    // read succeeds, the shape round-trips, and the count is
    // preserved. The dispatch outcomes themselves are covered by
    // unit tests in `crates/stage-publish/src/rollback_only.rs`.
    let updated = rollback_only::run(&mut ctx, "v0.0.0-test")
        .expect("rollback_only must read the persisted report.json");
    assert_eq!(updated.results.len(), 2);
    let mut updated_names: Vec<&str> = updated.results.iter().map(|r| r.name.as_str()).collect();
    updated_names.sort();
    assert_eq!(updated_names, vec!["assets-only", "manager-only"]);
    for r in &updated.results {
        // Either RolledBack (if a matching publisher were in the
        // registry) or RollbackFailed (registry mismatch — our case).
        // The assertion below pins "not Succeeded" to confirm
        // rollback_only actually walked the entries; an unchanged
        // `Succeeded` would mean the dispatcher didn't see them at
        // all, which would be a real bug.
        assert!(
            !matches!(r.outcome, PublisherOutcome::Succeeded),
            "rollback_only must flip {} away from Succeeded, got {:?}",
            r.name,
            r.outcome,
        );
    }

    // The replay also writes its own state alongside report.json —
    // verify the sibling file exists so a regression in the rollback-
    // side writer is caught at the same boundary.
    let rollback_path = tmp.path().join("run-v0.0.0-test").join("rollback.json");
    assert!(
        rollback_path.exists(),
        "expected rollback.json at {}",
        rollback_path.display()
    );
}

#[test]
fn writer_is_noop_when_no_publishers_ran() {
    // PublishStage with no synthetic publishers + no
    // set_publish_report -> writer must skip. Asserts the empty-state
    // contract that keeps dist/ clean in real "no publishers
    // configured" releases.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = TestContextBuilder::new()
        .tag("v0.0.0-test")
        .dist(tmp.path().to_path_buf())
        .build();
    let log = ctx.logger("publish-test-integration");
    write_report_to_run_dir(&ctx, &log);
    assert!(
        !tmp.path().join("run-v0.0.0-test").exists(),
        "no publishers ran -> no run-dir written"
    );
}
