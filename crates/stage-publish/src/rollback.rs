//! Publisher rollback engine and the `anodizer tag rollback` entrypoint.
//!
//! [`run`] loads the persisted publish state for a `run_id` — preferring
//! `<dist>/run-<id>/rollback.json` (a prior pass's state) over
//! `<dist>/run-<id>/report.json` (the original end-of-pipeline snapshot) —
//! and re-invokes each `Publisher`'s rollback for every
//! [`rollback_candidates`] row, writing the updated state back to
//! `<dist>/run-<id>/rollback.json`. Re-invoking `run` against the same
//! `run_id` is idempotent: a `RolledBack` entry from a prior pass matches
//! no candidate arm, while a `RollbackFailed` or `RollbackSkippedNoScope`
//! entry is re-attempted (the intended behavior — an operator who fixed a
//! transient failure or exported a missing scope var re-runs
//! `anodizer tag rollback` and expects those rows to retry).
//!
//! Two kinds of target are reverted:
//!
//! - every Assets/Manager publisher that successfully published
//!   (`PublisherOutcome::Succeeded`) — reverted via its API delete / PR
//!   close, transitioning the row to `RolledBack`;
//! - a *failed* required Submitter (cargo) that already pushed crates to
//!   crates.io — its recorded yank-target evidence drives the revert. The
//!   row KEEPS its `Failed` outcome on a successful yank (the release
//!   genuinely failed); only a yank failure moves it to `RollbackFailed`.
//!
//! Each rollback step is independent: a step's failure becomes
//! `RollbackFailed(err)` on its `PublisherResult`, but the next step still
//! runs. `anodizer tag rollback` is the sole caller — deliberate
//! withdrawal, once per deletable tag (`run_id` is that tag's string).
//! Forward recovery — a publish that merely failed, with nothing landed —
//! is re-running `anodizer release`; `Publisher::reconcile` makes that
//! convergent rather than duplicating work, so there is no separate
//! "resume" mode here.
//!
//! An operator who wants to force a full re-roll can delete
//! `<dist>/run-<id>/rollback.json` manually; the next invocation falls
//! through to `report.json` and treats every `Succeeded` entry as needing
//! rollback again.
//!
//! The on-disk reports at `<dist>/run-<id>/report.json` and
//! `<dist>/run-<id>/rollback.json` share the same schema — `serde_json`
//! of [`PublishReport`]. `report.json` is the immutable end-of-pipeline
//! snapshot from the original run; `rollback.json` is the mutable
//! replay-state file, overwritten on every invocation.

use anodizer_core::context::Context;
use anodizer_core::{PublishReport, Publisher, PublisherGroup, PublisherOutcome};
use anyhow::{Context as _, Result, anyhow};
use std::fs;
use std::path::PathBuf;

/// Load the prior run state for `run_id` and re-attempt rollback for every
/// candidate row (see [`rollback_candidates`]). Returns the updated
/// [`PublishReport`] and writes it to `<dist>/run-<id>/rollback.json`.
///
/// Errors only when the prior state file is missing or unparseable;
/// per-step rollback failures are recorded as `RollbackFailed` on the
/// result and do not abort the loop.
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
    // Defense-in-depth: an upstream CLI parser may already reject unsafe
    // `run_id` values at parse time, but `run_with_publishers` is
    // reachable via the `pub` [`run`] and might be reached by a future
    // programmatic caller that bypasses that parser. Re-validate here so
    // the rule lives at the same module as the path-join.
    validate_run_id(run_id)?;

    let log = ctx.logger("publish");

    // Prefer rollback.json from a prior invocation over the immutable
    // report.json from the original run. This makes re-invoking `run`
    // against the same `run_id` idempotent: the second invocation sees
    // `RolledBack` entries from the first pass and naturally filters them
    // out (they match no candidate arm). Without this, a second pass
    // would re-read the unchanged report.json and re-roll every Succeeded
    // entry — for git-revert-based publishers (homebrew / scoop / nix /
    // AUR), a second revert would revert-the-revert and re-publish the
    // broken artifact the operator is trying to remove.
    //
    // Corruption / version-mismatch on rollback.json surfaces a clear
    // error rather than silently falling back to report.json — falling
    // back would re-roll everything and is the exact regression this
    // guard exists for.
    let mut report = load_prior_state(ctx, run_id, Some(&log))?;

    // Stage-owned publishers (blob) are absent from `configured_publishers`
    // but own reversible remote state; resolve their seeded rows here so a
    // rollback deletes the mirrored objects instead of marking the row
    // `RollbackFailed("publisher not found")`.
    let aux = crate::registry::rollback_publishers(ctx);

    let target_indices = rollback_candidates(&report);

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

        // Resolution, retain-opt-out, scope gating, the rollback call, and
        // the `Failed`-keeps-its-outcome-on-successful-yank rule live in
        // `execute_rollback_step` so a future second caller cannot drift
        // from this one. This pass reverts an already-decided set
        // persisted from a prior run; the original trigger cause is not
        // in scope here (it lived in the process that produced the
        // report), so `on_rollback` fires with an empty `{{ .Reason }}`
        // rather than a fabricated one.
        let (outcome, disposition) =
            execute_rollback_step(&row, &evidence, publishers, &aux, ctx, "rollback", "");
        match disposition {
            RollbackDisposition::RolledBack => rolled_back += 1,
            RollbackDisposition::Failed => failed += 1,
            RollbackDisposition::NotFound => not_found += 1,
            RollbackDisposition::SkippedNoScope => skipped_no_scope += 1,
            RollbackDisposition::Retained => {}
        }
        report.results[i].outcome = outcome;
    }

    log.status(&format!(
        "rollback complete — {} rolled back, {} failed, {} not found, {} skipped-no-scope",
        rolled_back, failed, not_found, skipped_no_scope,
    ));

    // Persist the updated state to <dist>/run-<id>/rollback.json so the
    // operator has an audit trail of what was attempted on this pass.
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

/// Whether any persisted publisher state exists for `run_id` — either a
/// prior pass's `rollback.json` or the original run's `report.json`.
///
/// Callers that discover run ids from the filesystem (rather than being
/// handed one) use this to tell "this run recorded nothing to withdraw"
/// apart from "this run's state is unreadable": [`run`] and
/// [`planned_rollback_names`] both hard-error on a missing file, which is
/// the right contract for an explicit id but the wrong one for a sweep.
pub fn prior_state_exists(ctx: &Context, run_id: &str) -> bool {
    rollback_path(ctx, run_id).exists() || report_path(ctx, run_id).exists()
}

/// Publisher names [`run`] would attempt to withdraw for `run_id`, in
/// report order. Read-only: nothing is invoked and nothing is written, so
/// this is what a `--dry-run` withdrawal previews.
///
/// Same load contract as [`run`] — errors when the persisted state is
/// missing or unparseable.
pub fn planned_rollback_names(ctx: &Context, run_id: &str) -> Result<Vec<String>> {
    validate_run_id(run_id)?;
    let report = load_prior_state(ctx, run_id, None)?;
    Ok(rollback_candidates(&report)
        .into_iter()
        .map(|i| report.results[i].name.clone())
        .collect())
}

/// Load the persisted publisher state for `run_id`, preferring a prior
/// pass's `rollback.json` over the original `report.json`.
///
/// Preferring `rollback.json` makes a repeated withdrawal idempotent: the
/// second pass sees the first pass's `RolledBack` rows and filters them out
/// (they match no candidate arm). Without it, a second pass would re-read
/// the unchanged `report.json` and re-roll every `Succeeded` entry — for
/// the git-revert publishers (homebrew / scoop / nix / AUR) a second revert
/// would revert-the-revert and re-publish the artifact the operator is
/// removing.
///
/// Corruption on `rollback.json` surfaces a clear error rather than
/// silently falling back to `report.json` — falling back would re-roll
/// everything, the exact regression this ordering exists to prevent.
///
/// `log` is `Some` only for the executing path; the read-only preview
/// stays silent so a dry-run does not narrate file reads.
fn load_prior_state(
    ctx: &Context,
    run_id: &str,
    log: Option<&anodizer_core::log::StageLogger>,
) -> Result<PublishReport> {
    let prior_state = rollback_path(ctx, run_id);
    let (path, source_label, is_rollback_state) = if prior_state.exists() {
        if let Some(log) = log {
            log.status(&format!(
                "resuming from prior rollback state at {}",
                prior_state.display()
            ));
        }
        (prior_state, "prior rollback state", true)
    } else {
        let report = report_path(ctx, run_id);
        if let Some(log) = log {
            log.status(&format!(
                "loading prior report (first pass) from {}",
                report.display()
            ));
        }
        (report, "prior report", false)
    };

    let report_text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {} at {}", source_label, path.display()))?;
    serde_json::from_str(&report_text).with_context(|| {
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
    })
}

/// Resolve the path to the prior run's `report.json` under
/// `<ctx.config.dist>/run-<id>/report.json`. Delegates to the crate-level
/// [`crate::report_path_for`] so the read path and the writer in
/// `write_report_to_run_dir` share one path-shape definition.
fn report_path(ctx: &Context, run_id: &str) -> PathBuf {
    crate::report_path_for(ctx, run_id)
}

/// Resolve the path [`run`] writes its updated state to:
/// `<ctx.config.dist>/run-<id>/rollback.json`.
fn rollback_path(ctx: &Context, run_id: &str) -> PathBuf {
    crate::run_dir(ctx, run_id).join(anodizer_core::dist::ROLLBACK_JSON)
}

/// Validate that `run_id` is safe to join into a filesystem path.
///
/// `run_id` is either operator-supplied (`promote --from-run`) or
/// tag-derived (`anodizer tag rollback`'s publisher unwind) and is joined
/// into both a read path (`<dist>/run-<id>/report.json`) and a write path
/// (`<dist>/run-<id>/rollback.json`). Without validation, a malformed id
/// like `../../etc/passwd` would resolve to
/// `<dist>/run-../../etc/passwd/rollback.json` for the write — operator
/// data-loss potential.
///
/// Rules (single source of truth for any upstream caller):
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
/// [`run_with_publishers`] so a future programmatic caller bypassing an
/// upstream parser still gets the same rule.
pub fn validate_run_id(run_id: &str) -> Result<()> {
    // Single recovery-hint string reused across every error branch so the
    // operator sees a uniform "here's what a valid id looks like"
    // suggestion regardless of which rule they tripped.
    const HINT: &str = "(e.g. 'run-2026-05-14' or 'abc123')";

    if run_id.is_empty() {
        return Err(anyhow!("run id cannot be empty {}", HINT));
    }
    // `+` is here because a run id is normally the release tag, and anodize's
    // tag grammar admits a semver build metadata suffix (`v1.2.3+build.1`).
    // Without it the writer would silently fall back to the short commit for
    // exactly those tags while every reader still probes `run-<tag>/`.
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '+')
    {
        return Err(anyhow!(
            "run id '{}' contains invalid characters; allowed: [A-Za-z0-9._+-] {}",
            run_id,
            HINT
        ));
    }
    // Belt-and-suspenders against path-traversal segments. The char-set
    // check above already forbids `/` and `\`, but list them explicitly
    // so a reviewer scanning this function sees the intent.
    if run_id.contains('/') || run_id.contains('\\') {
        return Err(anyhow!(
            "run id '{}' must not contain path separators {}",
            run_id,
            HINT
        ));
    }
    if run_id == "." || run_id == ".." {
        return Err(anyhow!(
            "run id '{}' is not valid (path-traversal segment) {}",
            run_id,
            HINT
        ));
    }
    Ok(())
}

/// The single (group × outcome) rollback-candidacy predicate for [`run`],
/// returning the report indices to roll back.
///
/// - Assets/Manager: `Succeeded` (revert a recorded success via API
///   delete / PR close), plus `RollbackFailed` / `RollbackSkippedNoScope`
///   (retry a prior attempt that failed, or one that was blocked on a
///   scope env var the operator may since have exported).
/// - Submitter: a *failed* required Submitter (cargo) that already
///   pushed remote state still has a real yank to run, plus the same
///   `RollbackFailed` / `RollbackSkippedNoScope` retry arm. Every other
///   Submitter outcome — `Succeeded`, `Skipped`, etc. — is not a
///   candidate; Submitter rollback exists only to undo a partial
///   publish, not to revert a clean one.
///
/// `Succeeded` rows without recorded evidence are still candidates —
/// [`run`]'s loop surfaces the gap as
/// `RollbackFailed("no evidence in prior report")` rather than silently
/// leaving the row untouched, so a missing-evidence row is visible to the
/// operator instead of stranded.
pub(crate) fn rollback_candidates(report: &PublishReport) -> Vec<usize> {
    report
        .results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let candidate = match r.group {
                PublisherGroup::Assets | PublisherGroup::Manager => matches!(
                    r.outcome,
                    PublisherOutcome::Succeeded
                        | PublisherOutcome::RollbackFailed(_)
                        | PublisherOutcome::RollbackSkippedNoScope
                ),
                PublisherGroup::Submitter => matches!(
                    r.outcome,
                    PublisherOutcome::Failed(_)
                        | PublisherOutcome::RollbackFailed(_)
                        | PublisherOutcome::RollbackSkippedNoScope
                ),
            };
            candidate.then_some(i)
        })
        .collect()
}

/// How a single target resolved, so [`run`] can keep its own summary
/// counters without re-deriving them from the lossy [`PublisherOutcome`]
/// mapping.
pub(crate) enum RollbackDisposition {
    RolledBack,
    Failed,
    NotFound,
    SkippedNoScope,
    Retained,
}

/// Roll back ONE recorded publisher result and return its new outcome plus a
/// disposition for counting.
///
/// Resolves the publisher by name across the `publishers` list AND the
/// stage-owned `aux` list (blob, which owns `BlobStage` rather than a
/// dispatch entry), honors `retain_on_rollback`, gates on
/// `rollback_scope_needed`, then invokes [`Publisher::rollback`] and maps
/// the result. `prefix` labels the scope-unavailable warning. `reason` is
/// the run-wide rollback trigger cause forwarded to every `on_rollback`
/// firing as `{{ .Reason }}`; [`run`] always passes empty — the trigger
/// cause lived in the process that produced the persisted report, not
/// this one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_rollback_step(
    row: &anodizer_core::PublisherResult,
    evidence: &anodizer_core::PublishEvidence,
    publishers: &[Box<dyn Publisher>],
    aux: &[Box<dyn Publisher>],
    ctx: &mut Context,
    prefix: &str,
    reason: &str,
) -> (PublisherOutcome, RollbackDisposition) {
    let name = row.name.as_str();
    let current = &row.outcome;
    let log = ctx.logger("publish");
    let Some(publisher) = publishers
        .iter()
        .chain(aux.iter())
        .find(|p| p.name() == name)
    else {
        log.warn(&format!(
            "skipped rollback for '{name}' — publisher not in current registry"
        ));
        return (
            PublisherOutcome::RollbackFailed("publisher not found in current registry".into()),
            RollbackDisposition::NotFound,
        );
    };

    // Publisher opted out of rollback — leave its work (and outcome) in place.
    if publisher.retain_on_rollback() {
        log.status(&format!(
            "skipped rollback for '{name}' — retain_on_rollback is set"
        ));
        return (current.clone(), RollbackDisposition::Retained);
    }

    if let Some(label) = publisher.rollback_scope_needed()
        && !crate::scope::scope_available_with_env(label, ctx.env_source())
    {
        log.warn(&crate::scope::warn_scope_unavailable_msg(
            prefix, name, label,
        ));
        return (
            PublisherOutcome::RollbackSkippedNoScope,
            RollbackDisposition::SkippedNoScope,
        );
    }

    // A failed Submitter (cargo) keeps its `Failed` outcome on a SUCCESSFUL
    // yank: the release genuinely failed (crate B never went live) and
    // reporting `RolledBack` would mask that. Only a succeeded-then-reverted
    // Assets/Manager publisher transitions to `RolledBack`. A yank FAILURE
    // transitions to `RollbackFailed` for both — a live artifact we could not
    // pull, the manual-intervention signal.
    let was_failure = matches!(current, PublisherOutcome::Failed(_));
    log.status(&format!("invoking rollback for '{name}'"));
    match publisher.rollback(ctx, evidence) {
        Ok(()) => {
            let outcome = if was_failure {
                current.clone()
            } else {
                PublisherOutcome::RolledBack
            };
            // The publisher's own state was reverted — including a
            // `Succeeded`-then-reverted publisher whose on_error never fires.
            // Empty error: a clean revert has no failure message.
            crate::failure_hooks::fire_on_rollback(
                ctx,
                name,
                row.group,
                row.required,
                false,
                "",
                reason,
                &log,
            );
            (outcome, RollbackDisposition::RolledBack)
        }
        Err(err) => {
            let msg = format!("{:#}", err);
            log.warn(&format!("rollback for '{name}' failed: {msg}"));
            // A live artifact anodizer could not pull — the on_rollback hook is
            // the escalation surface (`{{ .RollbackFailed }}` == true).
            crate::failure_hooks::fire_on_rollback(
                ctx,
                name,
                row.group,
                row.required,
                true,
                &msg,
                reason,
                &log,
            );
            (
                PublisherOutcome::RollbackFailed(msg),
                RollbackDisposition::Failed,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Scope-availability tests inject a closed `MapEnvSource` on the test
    //! `Context` (read through `scope_available_with_env(ctx.env_source())`)
    //! rather than mutating process env, so the suite is hermetic and runs
    //! fully in parallel — no `serial_test` group is required.
    use super::*;
    use crate::testing::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{
        PublishEvidence, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };
    use tempfile::TempDir;

    /// Build a `Context` whose `config.dist` points at a fresh tempdir.
    /// Returns the context AND the `TempDir` guard so the directory
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

    /// Write a fixture report to `<dist>/run-<id>/report.json`.
    fn write_fixture_report(ctx: &Context, run_id: &str, report: &PublishReport) {
        let path = report_path(ctx, run_id);
        std::fs::create_dir_all(path.parent().unwrap()).expect("create run dir");
        let text = serde_json::to_string_pretty(report).expect("serialize report");
        std::fs::write(&path, text).expect("write fixture report");
    }

    /// Build a [`PublisherResult`] entry with `Succeeded` + matching
    /// `PublishEvidence`, mirroring what `dispatch()` writes for a
    /// successful publisher.
    fn succeeded(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(PublishEvidence::new(name)),
        }
    }

    /// Helper for a failed entry (no evidence, mirrors `dispatch`).
    fn failed(name: &str, group: PublisherGroup, required: bool, msg: &str) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required,
            outcome: PublisherOutcome::Failed(msg.into()),
            evidence: None,
        }
    }

    #[test]
    fn rollback_runs_for_succeeded_assets_and_manager() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("assets1", PublisherGroup::Assets, false));
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        // Submitter - even succeeded should NOT be rolled back.
        report
            .results
            .push(succeeded("sub1", PublisherGroup::Submitter, false));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![
            fake(
                "assets1",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake("mgr1", PublisherGroup::Manager, true, FakeOutcome::Succeed),
            fake(
                "sub1",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
        assert!(matches!(
            updated.results[1].outcome,
            PublisherOutcome::RolledBack
        ));
        // Submitter entry must remain Succeeded - rollback should not
        // touch it.
        assert!(matches!(
            updated.results[2].outcome,
            PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn rollback_step_failure_does_not_abort_siblings() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("first", PublisherGroup::Manager, true));
        report
            .results
            .push(succeeded("middle", PublisherGroup::Manager, true));
        report
            .results
            .push(succeeded("third", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers = vec![
            fake("first", PublisherGroup::Manager, true, FakeOutcome::Succeed),
            fake_with_rollback(
                "middle",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Succeed,
                FakeRollback::Fail("rollback bang".into()),
            ),
            fake("third", PublisherGroup::Manager, true, FakeOutcome::Succeed),
        ];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
        match &updated.results[1].outcome {
            PublisherOutcome::RollbackFailed(msg) => assert!(msg.contains("rollback bang")),
            other => panic!("expected RollbackFailed for middle, got {:?}", other),
        }
        assert!(matches!(
            updated.results[2].outcome,
            PublisherOutcome::RolledBack
        ));
    }

    #[test]
    fn rollback_skips_when_no_scope_available() {
        let (mut ctx, _tmp) = ctx_with_dist();
        // Inject an empty env source so the scope reads as unset through
        // `scope_available_with_env(ctx.env_source())` — no process-env
        // mutation, so the test is hermetic and needs no serial group.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "SCOPE_MISSING_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RollbackSkippedNoScope
        ));
    }

    #[test]
    fn rollback_records_failure_when_evidence_missing() {
        // A publisher recorded Succeeded but somehow lacks evidence
        // (defensive: the dispatcher always writes evidence for
        // Succeeded, but a hand-edited report.json could omit it). The
        // row is still a candidate — the loop surfaces the gap rather
        // than silently leaving a `Succeeded` row that was never
        // actually reverted.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "noevidence".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        write_fixture_report(&ctx, "fixt", &report);

        let publishers = vec![fake(
            "noevidence",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        match &updated.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("no evidence in prior report"))
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_reads_report_and_dispatches() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "succeeded entry should flip to RolledBack, got {:?}",
            updated.results[0].outcome,
        );
        let out = rollback_path(&ctx, "fixt");
        assert!(out.exists(), "rollback.json must be written");
    }

    #[test]
    fn rollback_marks_publisher_not_found_when_registry_lacks_it() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("orphan", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Empty registry — the report names a publisher we no longer have.
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

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

    /// Build a `Context` whose `config.dist` points at a fresh tempdir AND
    /// whose single crate declares a `blobs:` block so
    /// `registry::rollback_publishers` instantiates a `BlobPublisher`.
    fn ctx_with_blob_configured() -> (Context, TempDir) {
        use anodizer_core::config::{BlobConfig, CrateConfig};
        let tmp = TempDir::new().expect("create tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist dir");
        let crate_cfg = CrateConfig {
            name: "app".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "mirror".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v0.0.0-test")
            .dist(dist)
            .crates(vec![crate_cfg])
            .build();
        (ctx, tmp)
    }

    #[test]
    fn blob_row_resolves_via_rollback_publishers_not_marked_not_found() {
        // The blob-before-doors ordering seeds a Succeeded `blob` (Assets) row
        // into the report before rollback runs. `blob` is deliberately absent
        // from the dispatch registry (it owns BlobStage), so without
        // `registry::rollback_publishers` the loop would mark this row
        // RollbackFailed("publisher not found") and orphan the mirrored
        // objects. With blob configured it must resolve and roll back. (The
        // `succeeded` helper's evidence carries no structured blob_targets, so
        // BlobPublisher::rollback takes its hermetic empty-targets warn path —
        // no network — and returns Ok, flipping the row to RolledBack.)
        let (mut ctx, _tmp) = ctx_with_blob_configured();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("blob", PublisherGroup::Assets, true));
        write_fixture_report(&ctx, "fixt", &report);

        // No dispatch publishers: blob must be resolved from the aux list.
        let updated = run_with_publishers(&mut ctx, "fixt", &[]).expect("rollback");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "blob must resolve via rollback_publishers and roll back, got {:?}",
            updated.results[0].outcome
        );
    }

    #[test]
    fn blob_row_marked_not_found_when_blob_not_configured() {
        // Symmetry guard: `rollback_publishers` only instantiates a
        // BlobPublisher when blob is configured. With no `blobs:` block a
        // stray `blob` row genuinely has no owner and must surface as
        // RollbackFailed rather than silently passing.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("blob", PublisherGroup::Assets, true));
        write_fixture_report(&ctx, "fixt", &report);

        let updated = run_with_publishers(&mut ctx, "fixt", &[]).expect("rollback");

        match &updated.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("not found in current registry"))
            }
            other => panic!("expected RollbackFailed, got {:?}", other),
        }
    }

    #[test]
    fn rollback_honors_retain_on_rollback() {
        // A publisher with retain_on_rollback() = true must not have its
        // rollback() invoked, even if it has succeeded. Its outcome must
        // remain Succeeded after the rollback dispatcher runs.
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
                panic!("rollback() was called on a publisher with retain_on_rollback=true")
            }

            fn retain_on_rollback(&self) -> bool {
                true
            }
        }

        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("retain-pub", PublisherGroup::Assets, false));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(RetainPublisher)];
        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        // Outcome must remain Succeeded — rollback was skipped.
        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn rollback_bails_when_report_path_missing() {
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
    fn rollback_skips_non_succeeded_entries() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        // One Failed Manager entry (run() never succeeded; nothing to roll back).
        report
            .results
            .push(failed("failed-mgr", PublisherGroup::Manager, true, "boom"));
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

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

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
    fn rollback_writes_rollback_json() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        let out = rollback_path(&ctx, "fixt");
        let text = std::fs::read_to_string(&out).expect("read rollback.json");
        let parsed: PublishReport = serde_json::from_str(&text).expect("parse rollback.json");
        assert!(matches!(
            parsed.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
    }

    #[test]
    fn rollback_retries_rollback_failed_entries() {
        // RollbackFailed entries from a prior pass should be re-attempted —
        // that's the whole point of having the operator re-invoke
        // `anodizer tag rollback` after fixing whatever blocked the first pass.
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

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "RollbackFailed should re-attempt and flip to RolledBack on success, got {:?}",
            updated.results[0].outcome,
        );
    }

    #[test]
    fn rollback_records_failure_per_step() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![fake_with_rollback(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("rollback bang".into()),
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

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
            // Build-metadata tags: the rollback tag classifier accepts these,
            // so the run-id writer must too or writer and reader disagree.
            "v1.2.3+build.1",
            "mycrate-v1.2.3-rc.1+sha.abc",
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
        // Defense-in-depth: even though an upstream parser catches this,
        // run_with_publishers must reject too.
        let (mut ctx, _tmp) = ctx_with_dist();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        for bad in ["../etc/passwd", "foo/bar", ".."] {
            let err = run_with_publishers(&mut ctx, bad, &publishers)
                .expect_err("must reject unsafe run_id at entry point");
            let msg = format!("{:#}", err);
            assert!(
                msg.contains("run id"),
                "error should name the offending run id, got '{}'",
                msg
            );
        }
    }

    #[test]
    fn rollback_bails_when_report_unparseable() {
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
    // Idempotency-on-second-invocation tests.
    //
    // These exercise the rollback.json-preferred-over-report.json load path
    // that makes re-invoking `anodizer tag rollback` against the same
    // `run_id` safe.
    // -----------------------------------------------------------------------

    #[test]
    fn rollback_second_invocation_is_noop_for_already_rolled_back_entries() {
        // Re-invoking rollback must not re-roll entries that already
        // reached RolledBack on a prior pass.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let (publisher, counter) = fake_counting("mgr1", PublisherGroup::Manager, true);
        let publishers: Vec<Box<dyn Publisher>> = vec![publisher];

        // First pass: flips Succeeded → RolledBack via one rollback() call.
        let r1 = run_with_publishers(&mut ctx, "fixt", &publishers).expect("first pass");
        assert!(
            matches!(r1.results[0].outcome, PublisherOutcome::RolledBack),
            "first pass should flip Succeeded → RolledBack, got {:?}",
            r1.results[0].outcome,
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "first pass should have invoked rollback() exactly once",
        );

        // Second pass: must NOT re-invoke rollback() — the prior
        // rollback.json state shows the entry is already RolledBack.
        let r2 = run_with_publishers(&mut ctx, "fixt", &publishers).expect("second pass");
        assert!(
            matches!(r2.results[0].outcome, PublisherOutcome::RolledBack),
            "second pass should leave RolledBack as-is, got {:?}",
            r2.results[0].outcome,
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second pass must NOT have invoked rollback() again (counter should stay at 1)",
        );
    }

    #[test]
    fn rollback_retries_rollback_failed_entries_on_second_invocation() {
        // First pass leaves the entry as RollbackFailed (the publisher's
        // rollback() returned Err). A second pass must re-attempt it —
        // `RollbackFailed` IS in the filter set, so it gets dispatched.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // First pass: rollback() fails, leaving entry as RollbackFailed.
        let failing: Vec<Box<dyn Publisher>> = vec![fake_with_rollback(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("transient network blip".into()),
        )];
        let r1 = run_with_publishers(&mut ctx, "fixt", &failing).expect("first pass");
        match &r1.results[0].outcome {
            PublisherOutcome::RollbackFailed(msg) => {
                assert!(msg.contains("transient network blip"));
            }
            other => panic!("expected RollbackFailed after first pass, got {:?}", other),
        }

        // Second pass: same publisher name but rollback() now succeeds
        // (the operator fixed whatever blocked it). The RollbackFailed
        // entry from the persisted rollback.json must be re-attempted and
        // flip to RolledBack.
        let succeeding: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let r2 = run_with_publishers(&mut ctx, "fixt", &succeeding).expect("second pass");
        assert!(
            matches!(r2.results[0].outcome, PublisherOutcome::RolledBack),
            "second pass should re-attempt RollbackFailed and flip to RolledBack, got {:?}",
            r2.results[0].outcome,
        );
    }

    #[test]
    fn rollback_errors_on_unparseable_rollback_json() {
        // Corrupt rollback.json must surface a clear error rather than
        // silently falling back to report.json — that fallback would
        // re-roll every Succeeded entry, which is exactly the regression
        // we're guarding against.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
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
    fn rollback_falls_through_to_report_when_rollback_state_absent() {
        // Sanity: when rollback.json doesn't exist (first invocation), the
        // loader falls through to report.json and dispatches as usual.
        let (mut ctx, _tmp) = ctx_with_dist();
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("mgr1", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        // Precondition: rollback.json does NOT exist yet.
        assert!(!rollback_path(&ctx, "fixt").exists());

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "mgr1",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("first pass");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "fall-through should still dispatch and flip Succeeded → RolledBack",
        );
        // Postcondition: rollback.json now exists (the pass wrote it).
        assert!(rollback_path(&ctx, "fixt").exists());
    }

    // -----------------------------------------------------------------------
    // Scope-check parity: rollback must honor `rollback_scope_needed()` the
    // same way for every pass — otherwise it invokes `publisher.rollback(...)`
    // against a host that's missing the credential, which would either
    // fail-hard or (worse) silently degrade to no-op for a publisher that
    // swallows auth errors.
    // -----------------------------------------------------------------------

    #[test]
    fn rollback_does_not_invoke_rollback_when_scope_unavailable() {
        // Regression guard: when the scope check fires, the publisher's
        // `rollback()` must NOT be called. A fake with BOTH a non-None
        // scope AND a failing rollback lets the assertion distinguish
        // "scope-check honored" (RollbackSkippedNoScope) from "scope-check
        // skipped and rollback() actually ran" (RollbackFailed).
        let (mut ctx, _tmp) = ctx_with_dist();
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(crate::testing::FakePublisher {
            name: "scoped".into(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: FakeOutcome::Succeed,
            rollback_outcome: crate::testing::FakeRollback::Fail(
                "rollback() must not be called".into(),
            ),
            rollback_scope: Some("SCOPE_GUARD_TOKEN write"),
            skips_on_nightly: false,
            config_fully_inactive: false,
        })];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");
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
    fn rollback_proceeds_when_scope_available() {
        let (mut ctx, _tmp) = ctx_with_dist();
        ctx.set_env_source(anodizer_core::MapEnvSource::new().with("SCOPE_PRESENT_TOKEN", "xyz"));
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("scoped", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let publishers = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "SCOPE_PRESENT_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "available scope must allow rollback to proceed; got {:?}",
            updated.results[0].outcome,
        );
    }

    /// A row PERSISTED as `RollbackSkippedNoScope` (a prior pass ran
    /// without the scope env var and told the operator to export it and
    /// re-run) must be a candidate: once the scope is available the next
    /// invocation re-attempts the rollback and the row flips to
    /// `RolledBack`. Before candidacy was consolidated these rows could be
    /// stranded until the operator deleted `rollback.json` by hand.
    #[test]
    fn rollback_reattempts_rows_persisted_as_skipped_no_scope() {
        let (mut ctx, _tmp) = ctx_with_dist();
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new().with("SCOPE_REATTEMPT_TOKEN", "now-present"),
        );
        let mut report = PublishReport::default();
        let mut row = succeeded("scoped", PublisherGroup::Manager, true);
        row.outcome = PublisherOutcome::RollbackSkippedNoScope;
        report.results.push(row);
        write_fixture_report(&ctx, "fixt", &report);

        let publishers = vec![fake_with_scope(
            "scoped",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            "SCOPE_REATTEMPT_TOKEN write",
        )];

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");
        assert!(
            matches!(updated.results[0].outcome, PublisherOutcome::RolledBack),
            "a persisted RollbackSkippedNoScope row must be re-attempted once \
             the scope is available; got {:?}",
            updated.results[0].outcome,
        );
    }

    // -----------------------------------------------------------------------
    // on_rollback hook-firing mechanics.
    // -----------------------------------------------------------------------

    /// Build a Context whose `config.dist` points at `dist` and whose single
    /// crate declares an `on_rollback` hook that appends
    /// `<publisher> rf=<RollbackFailed>` (read from the process env, so no
    /// template-brace escaping) to `out` — the probe every on_rollback
    /// routing test asserts against.
    fn ctx_with_on_rollback_probe(out: &std::path::Path, dist: PathBuf) -> Context {
        use anodizer_core::config::{CrateConfig, HookEntry, PublishConfig, StructuredHook};
        let out_sh = out.display().to_string().replace('\\', "/");
        let publish = PublishConfig {
            on_rollback: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!(
                    "printf '%s\\n' \"$ANODIZER_PUBLISHER rf=$ANODIZER_ROLLBACK_FAILED\" >> {out_sh}"
                ),
                ..Default::default()
            })]),
            ..Default::default()
        };
        TestContextBuilder::new()
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                publish: Some(publish),
                ..Default::default()
            }])
            .build()
    }

    /// A non-firing disposition — a `retain_on_rollback: true` publisher, whose
    /// step returns before the firing seam — must fire ZERO `on_rollback` hooks.
    /// A probe-ABSENT assertion so a future refactor that relocates the fire
    /// calls above the early `return`s is caught.
    #[test]
    fn on_rollback_does_not_fire_for_retained_publisher() {
        struct RetainPublisher;
        impl Publisher for RetainPublisher {
            fn name(&self) -> &str {
                "retain-pub"
            }
            fn group(&self) -> PublisherGroup {
                PublisherGroup::Manager
            }
            fn required(&self) -> bool {
                true
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
                panic!("rollback() invoked on a retain_on_rollback=true publisher")
            }
            fn retain_on_rollback(&self) -> bool {
                true
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("rb.txt");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist dir");
        let mut ctx = ctx_with_on_rollback_probe(&out, dist);
        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(RetainPublisher)];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("retain-pub", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::Succeeded
        ));
        assert!(
            !out.exists(),
            "a retained (non-firing) disposition must fire zero on_rollback hooks"
        );
    }

    /// Headline case: a `Succeeded` publisher reverted fires `on_rollback`
    /// with `RollbackFailed=false`. This is the surface `on_error` cannot
    /// reach (the publisher never failed).
    #[test]
    fn on_rollback_fires_for_succeeded_then_reverted_publisher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("rb.txt");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist dir");
        let mut ctx = ctx_with_on_rollback_probe(&out, dist);
        let publishers = vec![fake(
            "homebrew",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("homebrew", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
        let body = std::fs::read_to_string(&out).expect("on_rollback hook must have run");
        assert_eq!(
            body.trim(),
            "homebrew rf=false",
            "on_rollback fires for the reverted publisher with RollbackFailed=false"
        );
    }

    /// A revert that itself fails fires `on_rollback` with
    /// `RollbackFailed=true` — the orphaned-artifact escalation surface.
    #[test]
    fn on_rollback_fires_with_rollback_failed_true_when_revert_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("rb.txt");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist dir");
        let mut ctx = ctx_with_on_rollback_probe(&out, dist);
        let publishers = vec![fake_with_rollback(
            "homebrew",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
            FakeRollback::Fail("tap delete rejected".into()),
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("homebrew", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RollbackFailed(_)
        ));
        let body = std::fs::read_to_string(&out).expect("on_rollback hook must have run");
        assert_eq!(body.trim(), "homebrew rf=true");
    }

    /// A publisher with no `on_rollback` configured must roll back cleanly with
    /// no hook side effect — the absent-hook path is a silent no-op.
    #[test]
    fn on_rollback_absent_is_noop() {
        let (mut ctx, _tmp) = ctx_with_dist();
        let publishers = vec![fake(
            "homebrew",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Succeed,
        )];
        let mut report = PublishReport::default();
        report
            .results
            .push(succeeded("homebrew", PublisherGroup::Manager, true));
        write_fixture_report(&ctx, "fixt", &report);

        let updated = run_with_publishers(&mut ctx, "fixt", &publishers).expect("rollback");

        assert!(matches!(
            updated.results[0].outcome,
            PublisherOutcome::RolledBack
        ));
    }
}
