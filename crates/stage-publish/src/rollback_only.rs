//! `--rollback-only --from-run=<id>` mode.
//!
//! Loads `<dist>/run-<id>/rollback.json` if it exists (a prior replay's
//! state) or falls through to `<dist>/run-<id>/report.json` (the original
//! end-of-pipeline snapshot written by the run-summary task) and
//! re-invokes each `Publisher`'s rollback for every `Succeeded` or
//! `RollbackFailed` Assets/Manager entry — PLUS any `Failed` /
//! `RollbackFailed` Submitter that opts in via
//! [`Publisher::programmatic_rollback_on_failure`] (cargo, whose
//! partial multi-crate publish left live crates to yank). Writes the
//! updated state back to `<dist>/run-<id>/rollback.json`.
//!
//! Re-invoking `--rollback-only` against the same `run_id` is idempotent:
//! `RolledBack` entries from a prior replay are naturally filtered out
//! (they match neither `Succeeded` nor `RollbackFailed`); `RollbackFailed`
//! entries are re-attempted (which is desired — a transient network
//! error from the first attempt deserves a retry).
//!
//! An operator who wants to force a full re-roll can delete
//! `<dist>/run-<id>/rollback.json` manually; the next replay will fall
//! through to `report.json` and treat every Succeeded entry as needing
//! rollback.
//!
//! No new publishing happens — the registry is loaded only to find the
//! matching `Publisher` impl per `result.name`. Entries that already
//! terminated as `RolledBack`, `Skipped(_)`, `PublishedNoRollback`, etc.
//! are left untouched, as are `Failed` Submitter entries that do NOT opt
//! into a programmatic rollback (every Submitter except cargo).
//!
//! The on-disk reports at `<dist>/run-<id>/report.json` and
//! `<dist>/run-<id>/rollback.json` share the same schema — `serde_json`
//! of [`PublishReport`]. `report.json` is the immutable end-of-pipeline
//! snapshot from the original run; `rollback.json` is the mutable
//! replay-state file, overwritten on every replay.

use anodizer_core::context::Context;
use anodizer_core::{PublishReport, Publisher, PublisherOutcome};
use anyhow::{Context as _, Result, anyhow};
use std::fs;
use std::path::PathBuf;

/// Validate that `run_id` is safe to join into a filesystem path.
///
/// `run_id` is operator-controlled (via `--from-run=<id>`) and is joined
/// into both a read path (`<dist>/run-<id>/report.json`) and a write
/// path (`<dist>/run-<id>/rollback.json`). Without validation,
/// `--from-run=../../etc/passwd` would resolve to
/// `<dist>/run-../../etc/passwd/rollback.json` for the write — operator
/// data-loss potential.
///
/// Rules (single source of truth; the CLI's `parse_run_id` delegates
/// here):
/// - Non-empty.
/// - All chars in `[A-Za-z0-9._-]`.
/// - No `/` or `\` (defense-in-depth; the char-set rule already forbids
///   them, but be explicit about path separators).
/// - No bare `.` or `..` segments (a literal `"."` matches the char-set
///   but is not a meaningful run id; `".."` is a path-traversal segment
///   even without a slash because some filesystems / `Path::join`
///   semantics treat it as a parent reference).
///
/// Defense-in-depth: this function is also called from
/// [`run_with_publishers`] so a future programmatic caller bypassing the
/// CLI parser still gets the same rule.
pub fn validate_run_id(run_id: &str) -> Result<()> {
    // Single recovery-hint string reused across every error branch so the
    // operator sees a uniform "here's what a valid id looks like"
    // suggestion regardless of which rule they tripped.
    const HINT: &str = "(e.g. 'run-2026-05-14' or 'abc123')";

    if run_id.is_empty() {
        return Err(anyhow!("--from-run cannot be empty {}", HINT));
    }
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "--from-run='{}' contains invalid characters; allowed: [A-Za-z0-9._-] {}",
            run_id,
            HINT
        ));
    }
    // Belt-and-suspenders against path-traversal segments. The char-set
    // check above already forbids `/` and `\`, but list them explicitly
    // so a reviewer scanning this function sees the intent.
    if run_id.contains('/') || run_id.contains('\\') {
        return Err(anyhow!(
            "--from-run='{}' must not contain path separators {}",
            run_id,
            HINT
        ));
    }
    if run_id == "." || run_id == ".." {
        return Err(anyhow!(
            "--from-run='{}' is not a valid run id (path-traversal segment) {}",
            run_id,
            HINT
        ));
    }
    Ok(())
}

/// Resolve the path to the prior run's `report.json` under
/// `<ctx.config.dist>/run-<id>/report.json`. Delegates to the crate-level
/// [`crate::report_path_for`] so the read path and the writer in
/// `write_report_to_run_dir` share one path-shape definition.
fn report_path(ctx: &Context, run_id: &str) -> PathBuf {
    crate::report_path_for(ctx, run_id)
}

/// Resolve the path the replay writes its updated state to:
/// `<ctx.config.dist>/run-<id>/rollback.json`.
fn rollback_path(ctx: &Context, run_id: &str) -> PathBuf {
    crate::run_dir(ctx, run_id).join(anodizer_core::dist::ROLLBACK_JSON)
}

/// Load the prior run state (preferring `<dist>/run-<id>/rollback.json`
/// from a prior replay over `<dist>/run-<id>/report.json` from the
/// original run) and re-attempt rollback for every `Succeeded` or
/// `RollbackFailed` Assets/Manager entry. Returns the updated
/// [`PublishReport`] and writes it to `<dist>/run-<id>/rollback.json`.
///
/// Errors only when the prior state file is missing or unparseable;
/// per-step rollback failures are recorded as `RollbackFailed` on the
/// result and do not abort the loop (mirrors the post-publish rollback
/// runner).
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
    // Defense-in-depth: the CLI layer's value_parser already rejects
    // unsafe `run_id` values at parse time, but `run_with_publishers` is
    // `pub` (via [`run`]) and might be reached by a future programmatic
    // caller that bypasses the CLI parser. Re-validate here so the rule
    // lives at the same module as the path-join.
    validate_run_id(run_id)?;

    let log = ctx.logger("publish");

    // Prefer rollback.json from a prior replay over the immutable
    // report.json from the original run. This makes
    // `--rollback-only --from-run=<id>` idempotent: the second invocation
    // sees `RolledBack` entries from the first replay and naturally
    // filters them out (they match neither `Succeeded` nor
    // `RollbackFailed`). Without this, the second replay would re-read
    // the unchanged report.json and re-roll every Succeeded entry — for
    // git-revert-based publishers (homebrew / scoop / nix / our-AUR), a
    // second revert would revert-the-revert and re-publish the broken
    // artifact the operator is trying to remove.
    //
    // Corruption / version-mismatch on rollback.json surfaces a clear
    // error rather than silently falling back to report.json — falling
    // back would re-roll everything and is the exact regression this
    // guard exists for.
    let prior_state = rollback_path(ctx, run_id);
    let (path, source_label, is_rollback_state) = if prior_state.exists() {
        log.status(&format!(
            "resuming (replay) from prior rollback state at {}",
            prior_state.display()
        ));
        (prior_state, "prior rollback state", true)
    } else {
        let report = report_path(ctx, run_id);
        log.status(&format!(
            "loading prior report (first run) from {}",
            report.display()
        ));
        (report, "prior report", false)
    };

    let report_text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {} at {}", source_label, path.display()))?;
    let mut report: PublishReport = serde_json::from_str(&report_text).with_context(|| {
        // For the rollback-state branch specifically, bake the recovery
        // hint into the error so the operator doesn't have to dig into
        // the module rustdoc or commit body to learn the escape hatch.
        // The report.json branch is a clean "no prior state" or
        // "pipeline-written file is corrupt" case where the recovery is
        // re-running the pipeline, not deleting a file.
        if is_rollback_state {
            format!(
                "failed to parse {} at {}; delete the file to force a full re-roll from report.json",
                source_label,
                path.display(),
            )
        } else {
            format!("failed to parse {} at {}", source_label, path.display())
        }
    })?;

    // Stage-owned publishers (blob) are absent from `configured_publishers`
    // but own reversible remote state; resolve their seeded rows here so a
    // `--rollback-only` replay deletes the mirrored objects instead of marking
    // the row `RollbackFailed("publisher not found")`. Mirrors `rollback::run`.
    let aux = crate::registry::rollback_publishers(ctx);
    let find_publisher = |name: &str| {
        publishers
            .iter()
            .chain(aux.iter())
            .find(|p| p.name() == name)
            .map(|b| b.as_ref())
    };

    // The candidacy policy — which (group × outcome) rows are re-attempted,
    // including the replay-only RollbackFailed retry and
    // RollbackSkippedNoScope re-attempt — is shared with the live pass via
    // `rollback::rollback_candidates` so the two filters cannot drift.
    let target_indices = crate::rollback::rollback_candidates(
        &report,
        crate::rollback::CandidateMode::Replay,
        find_publisher,
    );

    if target_indices.is_empty() {
        log.warn("no rollback-eligible entries in prior report; nothing to do");
    } else {
        log.status(&format!(
            "dispatching rollback for {} target(s)",
            target_indices.len()
        ));
    }

    let mut rolled_back = 0usize;
    let mut failed = 0usize;
    let mut not_found = 0usize;
    let mut skipped_no_scope = 0usize;

    for i in target_indices {
        let (row, evidence) = {
            let r = &report.results[i];
            (r.clone(), r.evidence.clone())
        };

        let Some(evidence) = evidence else {
            log.warn(&format!(
                "skipped rollback for '{}' — no evidence in prior report",
                row.name,
            ));
            failed += 1;
            report.results[i].outcome =
                PublisherOutcome::RollbackFailed("no evidence in prior report".into());
            continue;
        };

        // Resolution, retain-opt-out, scope gating, the rollback call, and the
        // `Failed`-keeps-its-outcome-on-successful-yank rule are shared with the
        // live path via `rollback::execute_rollback_step` so the two cannot
        // drift (the replay path previously skipped `retain_on_rollback`).
        let (outcome, disposition) = crate::rollback::execute_rollback_step(
            &row,
            &evidence,
            publishers,
            &aux,
            ctx,
            "rollback-only",
        );
        match disposition {
            crate::rollback::RollbackDisposition::RolledBack => rolled_back += 1,
            crate::rollback::RollbackDisposition::Failed => failed += 1,
            crate::rollback::RollbackDisposition::NotFound => not_found += 1,
            crate::rollback::RollbackDisposition::SkippedNoScope => skipped_no_scope += 1,
            crate::rollback::RollbackDisposition::Retained => {}
        }
        report.results[i].outcome = outcome;
    }

    log.status(&format!(
        "rollback complete — {} rolled back, {} failed, {} not found, {} skipped-no-scope",
        rolled_back, failed, not_found, skipped_no_scope,
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
    log.status(&format!("wrote {}", out_path.display()));

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
    fn rollback_only_honors_retain_on_rollback() {
        // Regression: the replay path once had its OWN rollback loop that did
        // NOT consult `retain_on_rollback`, so an operator `--rollback-only`
        // replay would tear down a publisher its config explicitly asked to
        // retain — diverging from the live path. Both paths now share
        // `rollback::execute_rollback_step`, so a retain publisher must keep
        // its `Succeeded` outcome and never have `rollback()` invoked.
        struct RetainPublisher;
        impl Publisher for RetainPublisher {
            fn name(&self) -> &str {
                "retain-pub"
            }
            fn group(&self) -> PublisherGroup {
                PublisherGroup::Assets
            }
            fn required(&self) -> bool {
                false
            }
            fn skips_on_nightly(&self) -> bool {
                false
            }
            fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
                Ok(PublishEvidence::new("retain-pub"))
            }
            fn rollback(
                &self,
                _ctx: &mut Context,
                _evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                panic!("rollback() invoked on a retain_on_rollback=true publisher during replay")
            }
            fn retain_on_rollback(&self) -> bool {
                true
            }
        }

        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("retain-pub", PublisherGroup::Assets, false));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(RetainPublisher)];
        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback-only");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::Succeeded),
            "retain_on_rollback publisher must keep Succeeded on replay, got {:?}",
            updated.results[0].outcome,
        );
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
    fn validate_run_id_rejects_path_traversal() {
        // Every form an attacker / typo might produce.
        for bad in [
            "",            // empty
            "../etc",      // classic traversal
            "../../etc",   // deeper traversal
            "foo/bar",     // forward slash
            "foo\\bar",    // backslash (windows)
            "/abs",        // absolute path
            "..",          // bare parent segment
            ".",           // bare current-dir segment
            "foo bar",     // whitespace (outside charset)
            "foo;rm",      // shell-metacharacter (outside charset)
            "foo\nbar",    // newline
            "foo\0bar",    // NUL
            "foo$bar",     // env-style
            "fixt#frag",   // '#' outside charset
            "\u{202e}foo", // unicode RLO (not ascii-alphanumeric)
        ] {
            assert!(
                validate_run_id(bad).is_err(),
                "validate_run_id should reject {:?}",
                bad
            );
        }
    }

    #[test]
    fn validate_run_id_accepts_normal_ids() {
        // Realistic shapes the writer side might produce.
        for good in [
            "abc123",
            "v1.2.3",
            "run-2026-05-14",
            "_local-test",
            "DEADBEEF",
            "a",           // single char is fine
            "...",         // multiple dots, no traversal segment
            "..-trailing", // ".." prefix but as part of a longer token
            "foo..bar",    // ".." embedded — not a segment
            "0",           // single digit
        ] {
            assert!(
                validate_run_id(good).is_ok(),
                "validate_run_id should accept {:?}",
                good
            );
        }
    }

    #[test]
    fn run_with_publishers_rejects_invalid_run_id() {
        // Defense-in-depth: even though the CLI parser catches this,
        // run_with_publishers must reject too.
        let (mut ctx, _tmp) = ctx_with_dist();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        for bad in ["../etc/passwd", "foo/bar", ".."] {
            let err = run_with_publishers(&mut ctx, bad, &publishers)
                .expect_err("must reject unsafe run_id at entry point");
            let msg = format!("{:#}", err);
            assert!(
                msg.contains("--from-run"),
                "error should reference --from-run, got '{}'",
                msg
            );
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
        // Regression guard: the report.json branch must NOT carry the
        // rollback-state-specific recovery hint. Deleting a corrupt
        // pipeline-written report.json doesn't recover the operator;
        // re-running the pipeline does. Mis-routing the hint here would
        // mislead the operator into deleting evidence of the original
        // run.
        assert!(
            !msg.contains("delete the file to force a full re-roll"),
            "report.json parse error must NOT carry the rollback-state recovery hint, got '{}'",
            msg,
        );
    }

    // -----------------------------------------------------------------------
    // Idempotency-on-replay tests.
    //
    // These exercise the rollback.json-preferred-over-report.json load path
    // that makes `--rollback-only --from-run=<id>` safe to re-invoke.
    // -----------------------------------------------------------------------

    #[test]
    fn rollback_only_second_invocation_is_noop_for_already_rolled_back_entries() {
        // Re-invoking --rollback-only must not re-roll entries that
        // already reached RolledBack on a prior replay.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let (publisher, counter) = fake_counting("mgr1", PublisherGroup::Manager, true);
        let publishers: Vec<Box<dyn Publisher>> = vec![publisher];

        // First replay: flips Succeeded → RolledBack via one rollback() call.
        let r1 = run_with_publishers(&mut ctx, "fixt", &publishers).expect("first replay");
        assert!(
            matches!(r1.results[0].outcome, PublisherOutcome::RolledBack),
            "first replay should flip Succeeded → RolledBack, got {:?}",
            r1.results[0].outcome,
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "first replay should have invoked rollback() exactly once",
        );

        // Second replay: must NOT re-invoke rollback() — the prior
        // rollback.json state shows the entry is already RolledBack.
        let r2 = run_with_publishers(&mut ctx, "fixt", &publishers).expect("second replay");
        assert!(
            matches!(r2.results[0].outcome, PublisherOutcome::RolledBack),
            "second replay should leave RolledBack as-is, got {:?}",
            r2.results[0].outcome,
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second replay must NOT have invoked rollback() again (counter should stay at 1)",
        );
    }

    #[test]
    fn rollback_only_retries_rollback_failed_entries_on_second_invocation() {
        // First replay leaves the entry as RollbackFailed (the publisher's
        // rollback() returned Err). A second replay must re-attempt it —
        // `RollbackFailed` IS in the filter set, so it gets dispatched.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // First replay: rollback() fails, leaving entry as RollbackFailed.
        let failing: Vec<Box<dyn Publisher>> = vec![fake_with_rollback(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("transient network blip".into()),
        )];
        let r1 = run_with_publishers(&mut ctx, "fixt", &failing).expect("first replay");
        match &r1.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("transient network blip"));
            }
            other => panic!(
                "expected RollbackFailed after first replay, got {:?}",
                other
            ),
        }

        // Second replay: same publisher name but rollback() now succeeds
        // (the operator fixed whatever blocked it). The RollbackFailed
        // entry from the persisted rollback.json must be re-attempted and
        // flip to RolledBack.
        let succeeding: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let r2 = run_with_publishers(&mut ctx, "fixt", &succeeding).expect("second replay");
        assert!(
            matches!(r2.results[0].outcome, PublisherOutcome::RolledBack),
            "second replay should re-attempt RollbackFailed and flip to RolledBack, got {:?}",
            r2.results[0].outcome,
        );
    }

    #[test]
    fn rollback_only_errors_on_unparseable_rollback_json() {
        // Corrupt rollback.json must surface a clear error rather than
        // silently falling back to report.json — that fallback would
        // re-roll every Succeeded entry, which is exactly the C5
        // regression we're guarding against.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Write garbage to rollback.json. It exists, so the loader picks
        // it up and tries to parse — which must fail loudly.
        let rb_path = rollback_path(&ctx, "fixt");
        std::fs::create_dir_all(rb_path.parent().unwrap()).unwrap();
        std::fs::write(&rb_path, "not-json-at-all").unwrap();

        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let err = run_with_publishers(&mut ctx, "fixt", &publishers)
            .expect_err("must error on unparseable rollback.json");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("prior rollback state"),
            "error must reference the rollback-state source label, got '{}'",
            msg,
        );
        assert!(
            msg.contains(rb_path.to_string_lossy().as_ref()),
            "error must name the rollback.json path, got '{}'",
            msg,
        );
        // The error must surface the recovery hint so operators don't
        // have to dig into the module rustdoc or commit body. This is
        // the rollback-state-specific branch; the report.json branch
        // does NOT carry this suffix (a corrupt pipeline-written file
        // isn't recovered by deleting it).
        assert!(
            msg.contains("delete the file to force a full re-roll"),
            "error must surface the recovery hint, got '{}'",
            msg,
        );
    }

    #[test]
    fn rollback_only_falls_through_to_report_when_rollback_state_absent() {
        // Sanity: when rollback.json doesn't exist (first invocation), the
        // loader falls through to report.json and dispatches as usual.
        // This is what `rollback_only_reads_report_and_dispatches` already
        // covers implicitly; this version makes the absence assertion
        // explicit.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Precondition: rollback.json does NOT exist yet.
        assert!(!rollback_path(&ctx, "fixt").exists());

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("first replay");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "fall-through should still dispatch and flip Succeeded → RolledBack",
        );
        // Postcondition: rollback.json now exists (the replay wrote it).
        assert!(rollback_path(&ctx, "fixt").exists());
    }

    // -----------------------------------------------------------------------
    // Scope-check parity with `rollback::run`.
    //
    // The replay path must honor `rollback_scope_needed()` the same way
    // the live rollback dispatcher does — otherwise the replay invokes
    // `publisher.rollback(...)` against a host that's missing the
    // credential, which would either fail-hard or (worse) silently
    // degrade to no-op for a publisher that swallows auth errors.
    // -----------------------------------------------------------------------

    #[test]
    #[serial_test::serial(scope_env)]
    fn rollback_only_skips_when_scope_unavailable_records_no_scope() {
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            // env-ok: #[serial(scope_env)]; unique per-test ROLLBACK_ONLY_* token
            std::env::remove_var("ROLLBACK_ONLY_SCOPE_TEST_TOKEN");
        }
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "ROLLBACK_ONLY_SCOPE_TEST_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("replay");
        assert!(
            matches!(
                updated.results[0].outcome,
                PublisherOutcome::RollbackSkippedNoScope,
            ),
            "missing scope must record RollbackSkippedNoScope, got {:?}",
            updated.results[0].outcome,
        );
    }

    #[test]
    #[serial_test::serial(scope_env)]
    fn rollback_only_does_not_invoke_rollback_when_scope_unavailable() {
        // Regression guard: when the scope check fires, the publisher's
        // `rollback()` must NOT be called. Using FakeCountingPublisher
        // would not work here because it has no scope hook — instead we
        // use a fake_with_rollback that errors on rollback, plus the
        // scope-bearing fake. If the scope check is honored, the
        // outcome flips to RollbackSkippedNoScope (no error from the
        // rollback impl). If the scope check is missing, the outcome
        // would flip to RollbackFailed (because rollback errored).
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            // env-ok: #[serial(scope_env)]; unique per-test ROLLBACK_ONLY_* token
            std::env::remove_var("ROLLBACK_ONLY_SCOPE_GUARD_TOKEN");
        }
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Construct a FakePublisher with BOTH a non-None scope AND a
        // failing rollback — so the test can distinguish "scope-check
        // honored" (RollbackSkippedNoScope) from "scope-check skipped
        // and rollback() actually ran" (RollbackFailed).
        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(crate::testing::FakePublisher {
            name: "scoped".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: FakeOutcome::Succeed,
            rollback_outcome: crate::testing::FakeRollback::Fail(
                "rollback() must not be called".into(),
            ),
            rollback_scope: Some("ROLLBACK_ONLY_SCOPE_GUARD_TOKEN write"),
            skips_on_nightly: false,
        })];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("replay");
        assert!(
            matches!(
                updated.results[0].outcome,
                PublisherOutcome::RollbackSkippedNoScope,
            ),
            "scope-check must short-circuit before rollback(); got {:?}",
            updated.results[0].outcome,
        );
    }

    #[test]
    #[serial_test::serial(scope_env)]
    fn rollback_only_proceeds_when_scope_available() {
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            // env-ok: #[serial(scope_env)]; unique per-test ROLLBACK_ONLY_* token
            std::env::set_var("ROLLBACK_ONLY_SCOPE_PRESENT_TOKEN", "xyz");
        }
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded_entry("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "ROLLBACK_ONLY_SCOPE_PRESENT_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("replay");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "available scope must allow rollback to proceed; got {:?}",
            updated.results[0].outcome,
        );
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            // env-ok: #[serial(scope_env)]; unique per-test ROLLBACK_ONLY_* token
            std::env::remove_var("ROLLBACK_ONLY_SCOPE_PRESENT_TOKEN");
        }
    }

    /// A row PERSISTED as `RollbackSkippedNoScope` (the live pass ran
    /// without the scope env var and told the operator to export it and
    /// re-run) must be a replay candidate: once the scope is available the
    /// replay re-attempts the rollback and the row flips to `RolledBack`.
    /// Before candidacy was shared with the live pass these rows matched
    /// neither replay arm and were stranded until the operator deleted
    /// `rollback.json` by hand.
    #[test]
    #[serial_test::serial(scope_env)]
    fn rollback_only_reattempts_rows_persisted_as_skipped_no_scope() {
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            // env-ok: #[serial(scope_env)]; unique per-test ROLLBACK_ONLY_* token
            std::env::set_var("ROLLBACK_ONLY_SCOPE_REATTEMPT_TOKEN", "now-present");
        }
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        let mut row = succeeded_entry("scoped", PublisherGroup::Manager, true);
        row.outcome = PublisherOutcome::RollbackSkippedNoScope;
        report.results.push(row);
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "ROLLBACK_ONLY_SCOPE_REATTEMPT_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("replay");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "a persisted RollbackSkippedNoScope row must be re-attempted once \
             the scope is available; got {:?}",
            updated.results[0].outcome,
        );
    }
}
