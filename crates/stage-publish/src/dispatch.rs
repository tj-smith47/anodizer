//! Group-aware publisher dispatch.
//!
//! [`dispatch`] iterates the registry's publishers in
//! [`crate::registry::group_dispatch_order`] order (Assets → Manager →
//! Submitter), recording a [`PublisherResult`] per publisher in the
//! returned [`PublishReport`].
//!
//! ## The one-way-door gate (a.k.a. the "submitter gate")
//!
//! Named for the immutable-registry Submitters it originally guarded, the
//! gate protects **every one-way-door group — Manager AND Submitter** —
//! not just Submitter. Both groups write to surfaces we cannot cleanly
//! reclaim: Manager pushes package-manager state a consumer may have
//! already pulled (homebrew tap, scoop bucket, nix-pkgs, AUR, MCP
//! registry); Submitter writes immutable registry slots / moderation
//! queues (cargo, chocolatey, winget, snapcraft, upstream-AUR). Only the
//! Assets group (blob, github-release, cloudsmith, gemfury, dockerhub,
//! artifactory) is left ungated — those failures are reversible via API
//! delete, so Assets runs best-effort and rolls back.
//!
//! The gate is re-evaluated before **each Manager- and Submitter-group
//! publisher** (not once at group entry) and fires when:
//!
//! 1. `opts.gate_submitter` is `true` (default), AND
//! 2. any **required** publisher in an already-run group failed — Assets
//!    (e.g. a required blob upload), an earlier Manager, **or a required
//!    Submitter that already ran earlier in the sequential Submitter
//!    loop** (cargo runs first; a required cargo failure must stop the
//!    later irreversible submitters).
//!
//! Both conditions are folded into the single authoritative
//! [`PublishReport::submitter_gate_closed`] predicate so the gate rule
//! cannot drift between the in-dispatch loop and the snapcraft stage that
//! runs as its own Submitter surface.
//!
//! When gated, the remaining Manager and Submitter publishers record
//! `Skipped(SubmitterGated)` instead of running and
//! `report.submitter_gated` is set to `true`. This is the load-bearing
//! protection against firing a one-way door past a known-broken release:
//! the "chocolatey moderation got submitted, then winget validation
//! failed and we can't undo the choco upload" failure mode, the "cargo
//! published crate A, failed on crate B, yet winget still submitted"
//! intra-Submitter variant, and — the case this gate was widened to
//! cover — the "a required blob mirror upload failed, yet we still pushed
//! the homebrew tap / posted the MCP registry entry" Manager variant.
//! Prevention here is the only sound remedy: a Manager rollback is
//! best-effort (it silently no-ops when its scope credential is absent)
//! and cannot retract what a consumer has already pulled.
//!
//! `opts.fail_fast` stops iteration at the first publisher failure within
//! the current group; the partial report is still returned via `Ok`.
//! `Err` is reserved for catastrophic non-publisher errors (impossible
//! IO, malformed config); per-publisher failures land in the report.
//!
//! [`dispatch`] is the production publish path: `PublishStage::run` →
//! `run_with_publishers` calls it for every release, seeding its report from
//! any prior `ctx.publish_report` (e.g. the blob row) so the Submitter gate
//! observes upstream required failures.

use anodizer_core::context::Context;
use anodizer_core::{
    PublishReport, Publisher, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
};

/// Knobs for [`dispatch`].
///
/// `gate_submitter` defaults to `true`: it arms the one-way-door gate,
/// so **both** Manager and Submitter publishers are skipped once any
/// required publisher in an already-run group failed (the field keeps its
/// historical name — it originally gated only the Submitter group).
/// `fail_fast` defaults to `false`: the dispatcher continues past a
/// failed publisher so the resulting report enumerates every outcome for
/// the release summary.
///
/// `persist_snapshots` defaults to `false` (unit tests drive `dispatch`
/// with fixture contexts whose `dist` must stay untouched); the
/// production `PublishStage` sets it to `true` so
/// [`crate::run_summary::persist_summary_snapshot`] rewrites
/// `summary.json` after every publisher — a hard kill mid-publish then
/// still leaves the last-known publish state on disk for recovery
/// tooling.
#[derive(Debug, Clone, Copy)]
pub struct DispatchOptions {
    pub fail_fast: bool,
    pub gate_submitter: bool,
    pub persist_snapshots: bool,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            fail_fast: false,
            gate_submitter: true,
            persist_snapshots: false,
        }
    }
}

/// Dispatch publishers in Assets -> Manager -> Submitter order, applying
/// the Submitter gate when a required Assets/Manager publisher failed.
/// Returns Ok(partial-report) on per-publisher failure or fail-fast;
/// Err is reserved for catastrophic non-publisher errors.
pub fn dispatch(
    publishers: &[Box<dyn Publisher>],
    ctx: &mut Context,
    opts: &DispatchOptions,
) -> anyhow::Result<PublishReport> {
    // Best-effort durable snapshot of the in-progress report; a write
    // failure must never fail the publish itself (the summary is a
    // secondary observability channel), so it degrades to a warning.
    let snapshot = |ctx: &Context, report: &PublishReport| {
        if !opts.persist_snapshots {
            return;
        }
        if let Err(err) = crate::run_summary::persist_summary_snapshot(ctx, report) {
            ctx.logger("publish")
                .warn(&format!("summary snapshot write failed: {err:#}"));
        }
    };

    // Seed from any report an earlier Assets-group stage already wrote (the
    // BlobStage outcome, which now runs BEFORE PublishStage), rather than
    // discarding it with a fresh `default()`. A required-blob failure recorded
    // upstream must be visible to `submitter_gate_closed()` so the Submitter
    // loop gates the one-way doors (cargo / chocolatey / winget). `take()`
    // leaves `ctx.publish_report` `None`; `run_with_publishers` writes the
    // merged report back via `set_publish_report` after dispatch.
    let mut report = ctx.publish_report.take().unwrap_or_default();
    let group_order = crate::registry::group_dispatch_order();

    'outer: for group in group_order {
        for p in publishers.iter().filter(|p| p.group() == group) {
            // One-way-door gate, re-checked per publisher. It covers BOTH
            // the Manager and Submitter groups — every publisher that
            // writes a surface we cannot cleanly reclaim — while leaving
            // the reversible Assets group ungated. The gate closes when a
            // required publisher in ANY already-run group failed: a
            // required Assets failure (e.g. the blob mirror) that ran
            // upstream, a required Manager that failed earlier in this
            // loop, or a required Submitter that failed earlier (submitters
            // run sequentially, cargo first, so a required cargo failure
            // must stop the later irreversible submitters). Re-checking per
            // publisher (rather than once at group entry) is what makes the
            // intra-group ordering safe: each remaining one-way door
            // consults the live "any required publish already broke" state
            // via the single authoritative `submitter_gate_closed`
            // predicate. Gating Manager here — rather than firing it and
            // relying on rollback — is deliberate: a Manager rollback is
            // best-effort and cannot un-pull what a consumer already has.
            if matches!(group, PublisherGroup::Manager | PublisherGroup::Submitter)
                && opts.gate_submitter
                && report.submitter_gate_closed()
            {
                ctx.logger("publish").status(&format!(
                    "skipping {} — gated by an earlier required failure (one-way-door protection)",
                    p.name()
                ));
                report.results.push(PublisherResult {
                    name: p.name().into(),
                    group,
                    required: p.required(),
                    outcome: PublisherOutcome::Skipped(SkipReason::SubmitterGated),
                    evidence: None,
                });
                report.submitter_gated = true;
                snapshot(ctx, &report);
                continue;
            }

            // Nightly skip-list: publishers that opt out of `--nightly`
            // record `Skipped(Nightly)` and never invoke `run`. Matches
            // The documented nightlies skip set
            // (homebrew, scoop, aur, krew, nix, all announcers, gomod
            // proxy). Honoured before `simulate_failure` so the test
            // harness still observes the real nightly gate.
            if ctx.is_nightly() && p.skips_on_nightly() {
                ctx.logger("publish").status(&format!(
                    "skipping {} — not published on --nightly",
                    p.name()
                ));
                report.results.push(PublisherResult {
                    name: p.name().into(),
                    group,
                    required: p.required(),
                    outcome: PublisherOutcome::Skipped(SkipReason::Nightly),
                    evidence: None,
                });
                snapshot(ctx, &report);
                continue;
            }

            // Uniform operator-selection filter, evaluated at the single
            // dispatch chokepoint so EVERY publisher honours `--skip` and
            // `--publishers` — including non-stage publishers (npm,
            // dockerhub, uploads, …) that no stage-skip token ever covered.
            // `--skip` (denylist) always wins; a non-empty `--publishers`
            // (allowlist) deselects everything not listed. Both are folded
            // into `Context::publisher_deselected`. Recorded as
            // `Skipped(Deselected)` so the run summary counts it; never
            // silent (sibling skip branches above also log + snapshot +
            // continue).
            if ctx.publisher_deselected(p.name()) {
                let line = ctx.deselected_reason(p.name());
                ctx.logger("publish").status(&line);
                report.results.push(PublisherResult {
                    name: p.name().into(),
                    group,
                    required: p.required(),
                    outcome: PublisherOutcome::Skipped(SkipReason::Deselected),
                    evidence: None,
                });
                snapshot(ctx, &report);
                continue;
            }

            // Test-harness short-circuit: when the operator listed this
            // publisher in `--simulate-failure` (env-gated by
            // `ANODIZE_TEST_HARNESS=1` at the CLI layer), bypass
            // `p.run()` entirely and record a synthetic failure so the
            // gate / rollback / report paths can be exercised
            // deterministically without monkey-patching the publisher.
            let simulated = ctx
                .options
                .simulate_failure_publishers
                .iter()
                .any(|name| name == p.name());
            let (outcome, evidence) = if simulated {
                (
                    PublisherOutcome::Failed(format!("simulated failure: {}", p.name())),
                    None,
                )
            } else {
                // Drain any stale override / partial evidence before
                // invoking `run` so a prior publisher cannot bleed its
                // outcome or evidence forward.
                let _ = ctx.take_pending_outcome();
                let _ = ctx.take_pending_evidence();
                // Attribute any retry backoff this publisher incurs to its name
                // so the run summary can name the flaky remote. Serial dispatch
                // makes the scope exact — one publisher runs at a time.
                let _retry_scope = anodizer_core::retry::RetryScope::enter(p.name());
                match p.run(ctx) {
                    Ok(evidence) => {
                        // If `run` recorded an outcome override (e.g.
                        // chocolatey moderation skip or PR-already-exists
                        // skip) use it instead of the default `Succeeded`
                        // mapping so the summary table reflects the real
                        // terminal state.
                        let outcome = ctx
                            .take_pending_outcome()
                            .unwrap_or(PublisherOutcome::Succeeded);
                        // A successful run owns its return value; any stale
                        // partial-evidence slot is irrelevant.
                        let _ = ctx.take_pending_evidence();
                        (outcome, Some(evidence))
                    }
                    // A failing run may have done irreversible work before
                    // bailing (e.g. cargo published crate A then failed on
                    // crate B). Recover the partial evidence it stashed so
                    // rollback can unwind what actually went live.
                    Err(err) => (
                        PublisherOutcome::Failed(format!("{err:#}")),
                        ctx.take_pending_evidence(),
                    ),
                }
            };
            let failed = matches!(outcome, PublisherOutcome::Failed(_));
            let result = PublisherResult {
                name: p.name().into(),
                group,
                required: p.required(),
                outcome,
                evidence,
            };
            report.results.push(result);
            snapshot(ctx, &report);
            if failed && opts.fail_fast {
                break 'outer;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn empty_registry_yields_empty_report() {
        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn Publisher>> = Vec::new();
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok for empty input");
        assert!(report.results.is_empty());
        assert!(!report.submitter_gated);
    }

    /// Publisher whose `run()` proves the previous publisher's result was
    /// already durable on disk BEFORE this publisher started — i.e. the
    /// dispatch loop snapshots after each publisher, not only at the end.
    struct AssertsPriorSnapshot;

    impl Publisher for AssertsPriorSnapshot {
        fn name(&self) -> &str {
            "second"
        }
        fn group(&self) -> PublisherGroup {
            PublisherGroup::Manager
        }
        fn required(&self) -> bool {
            false
        }
        fn skips_on_nightly(&self) -> bool {
            false
        }
        fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
            let path = crate::run_summary::summary_path(ctx)
                .expect("summary path resolves for a real (non-snapshot) run");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("snapshot missing at {}: {e}", path.display()));
            let summary: crate::run_summary::RunSummary =
                serde_json::from_str(&raw).expect("snapshot parses as RunSummary");
            assert_eq!(
                summary.results.len(),
                1,
                "first publisher's result persisted"
            );
            assert_eq!(summary.results[0].name, "first");
            Ok(anodizer_core::PublishEvidence::new("second".to_string()))
        }
        fn rollback(
            &self,
            _ctx: &mut Context,
            _evidence: &anodizer_core::PublishEvidence,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn persist_snapshots_writes_summary_after_each_publisher() {
        let mut ctx = Context::test_fixture();
        let dist = tempfile::tempdir().expect("tempdir");
        ctx.config.dist = dist.path().to_path_buf();

        let publishers: Vec<Box<dyn Publisher>> = vec![
            fake("first", PublisherGroup::Assets, false, FakeOutcome::Succeed),
            Box::new(AssertsPriorSnapshot),
        ];
        let opts = DispatchOptions {
            persist_snapshots: true,
            ..DispatchOptions::default()
        };
        let report = dispatch(&publishers, &mut ctx, &opts).expect("dispatch succeeds");
        assert_eq!(report.results.len(), 2);

        let path = crate::run_summary::summary_path(&ctx).expect("summary path resolves");
        let raw = std::fs::read_to_string(&path).expect("final snapshot exists");
        let summary: crate::run_summary::RunSummary =
            serde_json::from_str(&raw).expect("final snapshot parses");
        assert_eq!(summary.results.len(), 2);
    }

    #[test]
    fn snapshots_disabled_by_default_leave_dist_untouched() {
        let mut ctx = Context::test_fixture();
        let dist = tempfile::tempdir().expect("tempdir");
        ctx.config.dist = dist.path().to_path_buf();

        let publishers: Vec<Box<dyn Publisher>> = vec![fake(
            "first",
            PublisherGroup::Assets,
            false,
            FakeOutcome::Succeed,
        )];
        dispatch(&publishers, &mut ctx, &DispatchOptions::default()).expect("dispatch succeeds");

        let entries: Vec<_> = std::fs::read_dir(dist.path())
            .expect("read_dir dist")
            .collect();
        assert!(
            entries.is_empty(),
            "default dispatch options must not write into dist: {entries:?}"
        );
    }

    #[test]
    fn group_order_is_assets_manager_submitter() {
        let mut ctx = Context::test_fixture();
        // Intentionally registered out of order to prove the dispatcher
        // re-orders by group rather than relying on input order.
        let publishers = vec![
            fake(
                "sub",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["assets", "manager", "sub"]);
        assert!(!report.submitter_gated);
    }

    #[test]
    fn submitter_gate_skips_submitter_when_required_manager_fails() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(matches!(
            submitter.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));
        assert!(submitter.evidence.is_none());
    }

    #[test]
    fn required_submitter_failure_gates_later_irreversible_submitter() {
        // The intra-Submitter gate: submitters run sequentially. cargo
        // (Submitter, required) fails; the later irreversible submitter
        // (winget) must be skipped via the gate, NOT run against a release
        // whose required cargo publish already broke. fail_fast stays false
        // (the default) so the gate — not fail-fast — is what stops winget.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "cargo",
                PublisherGroup::Submitter,
                true,
                FakeOutcome::Fail("cargo crate-b failed after crate-a published".into()),
            ),
            fake(
                "winget",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(report.submitter_gated, "the gate must have closed");

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo entry present");
        assert!(
            matches!(cargo.outcome, PublisherOutcome::Failed(_)),
            "cargo ran and failed, got {:?}",
            cargo.outcome
        );

        let winget = report
            .results
            .iter()
            .find(|r| r.name == "winget")
            .expect("winget entry present");
        assert!(
            matches!(
                winget.outcome,
                PublisherOutcome::Skipped(SkipReason::SubmitterGated)
            ),
            "winget must be gated by the earlier required-cargo failure, got {:?}",
            winget.outcome
        );
    }

    #[test]
    fn seeded_required_assets_failure_gates_all_submitter_doors() {
        // The release-critical ordering invariant: BlobStage (Assets, required)
        // runs BEFORE PublishStage and records its outcome into
        // `ctx.publish_report`. The dispatcher must SEED its report from that
        // recorded outcome (not start from a fresh `default()`), so a
        // required-blob upload failure closes `submitter_gate_closed()` and
        // every one-way door — cargo, chocolatey, winget — is
        // `Skipped(SubmitterGated)` and never runs against a release whose
        // required blob upload already failed.
        //
        // Reverting the dispatch seed (`PublishReport::default()`) OR reordering
        // BlobStage back after PublishStage both regress the burned-release bug;
        // this test goes red the moment the seed is dropped — the doors run.
        let mut ctx = Context::test_fixture();
        let mut seeded = PublishReport::default();
        seeded.results.push(PublisherResult {
            name: "blob".into(),
            group: PublisherGroup::Assets,
            required: true,
            outcome: PublisherOutcome::Failed("minio upload refused: connection reset".into()),
            evidence: None,
        });
        ctx.publish_report = Some(seeded);

        let publishers = vec![
            fake(
                "cargo",
                PublisherGroup::Submitter,
                true,
                FakeOutcome::Succeed,
            ),
            fake(
                "chocolatey",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "winget",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(
            report.submitter_gated,
            "a required-blob (Assets) failure recorded before dispatch must close the gate"
        );

        for door in ["cargo", "chocolatey", "winget"] {
            let r = report
                .results
                .iter()
                .find(|r| r.name == door)
                .unwrap_or_else(|| panic!("{door} entry present"));
            assert!(
                matches!(
                    r.outcome,
                    PublisherOutcome::Skipped(SkipReason::SubmitterGated)
                ),
                "{door} must be submitter-gated by the seeded required-blob failure, got {:?}",
                r.outcome
            );
        }

        // The seeded blob row survives into the merged report (the dispatcher
        // appends to it rather than discarding it).
        let blob = report
            .results
            .iter()
            .find(|r| r.name == "blob")
            .expect("seeded blob row preserved in merged report");
        assert!(matches!(blob.outcome, PublisherOutcome::Failed(_)));
    }

    #[test]
    fn seeded_required_assets_failure_gates_manager_oneway_doors() {
        // F1 regression: the one-way-door gate must cover the Manager group,
        // not just Submitter. A required blob (Assets) upload failure recorded
        // before dispatch must SKIP homebrew/scoop/nix/aur/mcp (Manager,
        // one-way doors) — not fire them and rely on a best-effort rollback.
        // Before the fix the gate was scoped to `group == Submitter`, so these
        // Manager doors published against an incomplete release: the live
        // v0.13.1 incident where blob failed yet the homebrew tap push and MCP
        // registry POST still went out. This test goes red the moment the gate
        // is narrowed back to Submitter-only.
        let mut ctx = Context::test_fixture();
        let mut seeded = PublishReport::default();
        seeded.results.push(PublisherResult {
            name: "blob".into(),
            group: PublisherGroup::Assets,
            required: true,
            outcome: PublisherOutcome::Failed("minio upload refused: connection reset".into()),
            evidence: None,
        });
        ctx.publish_report = Some(seeded);

        let publishers = vec![
            fake(
                "homebrew",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Succeed,
            ),
            fake("mcp", PublisherGroup::Manager, false, FakeOutcome::Succeed),
            fake(
                "cargo",
                PublisherGroup::Submitter,
                true,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(report.submitter_gated);

        for door in ["homebrew", "mcp", "cargo"] {
            let r = report
                .results
                .iter()
                .find(|r| r.name == door)
                .unwrap_or_else(|| panic!("{door} entry present"));
            assert!(
                matches!(
                    r.outcome,
                    PublisherOutcome::Skipped(SkipReason::SubmitterGated)
                ),
                "{door} (one-way door) must be gated by the seeded required-blob failure, got {:?}",
                r.outcome
            );
        }
    }

    #[test]
    fn required_manager_failure_gates_later_manager_oneway_door() {
        // Intra-Manager gate: Manager publishers run sequentially, so a
        // required Manager failure must stop the LATER Manager one-way doors
        // (and every Submitter), not just Submitters. homebrew (required)
        // fails; scoop (a later Manager) must be gated, not pushed.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "homebrew",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("tap push rejected".into()),
            ),
            fake(
                "scoop",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(
            report.submitter_gated,
            "a required Manager failure must close the gate"
        );

        let homebrew = report
            .results
            .iter()
            .find(|r| r.name == "homebrew")
            .expect("homebrew entry present");
        assert!(
            matches!(homebrew.outcome, PublisherOutcome::Failed(_)),
            "homebrew ran and failed, got {:?}",
            homebrew.outcome
        );

        let scoop = report
            .results
            .iter()
            .find(|r| r.name == "scoop")
            .expect("scoop entry present");
        assert!(
            matches!(
                scoop.outcome,
                PublisherOutcome::Skipped(SkipReason::SubmitterGated)
            ),
            "scoop (later Manager one-way door) must be gated by the earlier \
             required-homebrew failure, got {:?}",
            scoop.outcome
        );
    }

    #[test]
    fn dispatch_starts_empty_when_no_report_seeded() {
        // Counterpart to the seed test: with no report already in ctx (the
        // `--publish` subset / no-blob case), the dispatcher starts empty and
        // an all-success Submitter run leaves the gate open.
        let mut ctx = Context::test_fixture();
        assert!(ctx.publish_report.is_none());
        let publishers = vec![fake(
            "cargo",
            PublisherGroup::Submitter,
            true,
            FakeOutcome::Succeed,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(!report.submitter_gated);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].name, "cargo");
    }

    #[test]
    fn optional_submitter_failure_does_not_gate_later_submitter() {
        // Continue-on-error preserved at the intra-Submitter level: an
        // OPTIONAL submitter failure must NOT close the gate, so the later
        // submitter still runs.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "cargo",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Fail("optional cargo boom".into()),
            ),
            fake(
                "winget",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(
            !report.submitter_gated,
            "an optional submitter failure must not close the gate"
        );

        let winget = report
            .results
            .iter()
            .find(|r| r.name == "winget")
            .expect("winget entry present");
        assert!(
            matches!(winget.outcome, PublisherOutcome::Succeeded),
            "winget must still run after an optional cargo failure, got {:?}",
            winget.outcome
        );
    }

    #[test]
    fn gate_is_scoped_per_crate_run_in_per_crate_mode() {
        // Per-crate workspace mode runs the publish pipeline once per
        // published crate, each invocation building a FRESH report scoped to
        // that crate's `selected_crates`. The gate reads only the live report
        // for the current run, so a required-Submitter failure while scoped
        // to one crate gates the remaining irreversible submitters in THAT
        // run — and a separate, clean run for a different crate is unaffected
        // (fresh report ⇒ open gate). This pins the recurring per-crate-mode
        // failure family: the gate must never bleed state across crate runs.
        let mut ctx = Context::test_fixture();
        ctx.options.selected_crates = vec!["cfgd-core".to_string()];
        let failing_run = vec![
            fake(
                "cargo",
                PublisherGroup::Submitter,
                true,
                FakeOutcome::Fail("cfgd-core cargo publish failed".into()),
            ),
            fake(
                "winget",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&failing_run, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(report.submitter_gated, "this crate's run must be gated");
        let winget = report
            .results
            .iter()
            .find(|r| r.name == "winget")
            .expect("winget entry present");
        assert!(matches!(
            winget.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));

        // A subsequent crate's run gets a brand-new report ⇒ the gate is
        // open again; no cross-run state leaks.
        let mut ctx2 = Context::test_fixture();
        ctx2.options.selected_crates = vec!["cfgd".to_string()];
        let clean_run = vec![fake(
            "cargo",
            PublisherGroup::Submitter,
            true,
            FakeOutcome::Succeed,
        )];
        let report2 = dispatch(&clean_run, &mut ctx2, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(
            !report2.submitter_gated,
            "a clean per-crate run must not inherit the prior run's closed gate"
        );
    }

    #[test]
    fn all_success_runs_every_publisher_ungated() {
        // The happy path: nothing failed, the gate stays open, every group
        // member runs.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "github-release",
                PublisherGroup::Assets,
                true,
                FakeOutcome::Succeed,
            ),
            fake(
                "homebrew",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Succeed,
            ),
            fake(
                "cargo",
                PublisherGroup::Submitter,
                true,
                FakeOutcome::Succeed,
            ),
            fake(
                "winget",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(!report.submitter_gated);
        assert!(
            report
                .results
                .iter()
                .all(|r| matches!(r.outcome, PublisherOutcome::Succeeded)),
            "every publisher should have run and succeeded: {:?}",
            report.results
        );
    }

    #[test]
    fn submitter_runs_when_only_optional_publishers_fail() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("optional manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert!(!report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(matches!(submitter.outcome, PublisherOutcome::Succeeded));
    }

    #[test]
    fn no_gate_submitter_runs_submitter_anyway() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("required manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let opts = DispatchOptions {
            fail_fast: false,
            gate_submitter: false,
            ..DispatchOptions::default()
        };
        let report = dispatch(&publishers, &mut ctx, &opts).expect("dispatch returns Ok");
        assert!(!report.submitter_gated);

        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(
            matches!(submitter.outcome, PublisherOutcome::Succeeded),
            "submitter should run when gate_submitter=false even on required-manager failure"
        );
    }

    #[test]
    fn fail_fast_aborts_at_first_error() {
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake("m1", PublisherGroup::Manager, false, FakeOutcome::Succeed),
            fake(
                "m2",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("boom".into()),
            ),
            fake("m3", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        ];
        let opts = DispatchOptions {
            fail_fast: true,
            gate_submitter: true,
            ..DispatchOptions::default()
        };
        let report = dispatch(&publishers, &mut ctx, &opts)
            .expect("fail_fast still returns Ok with the partial report");

        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["m1", "m2"], "m3 must not have run");
        assert!(report.results.iter().all(|r| r.name != "m3"));
    }

    #[test]
    fn dispatch_records_pending_moderation_when_run_recorded_override() {
        // Mirrors chocolatey's in-moderation skip path: `run()` returns
        // `Ok(evidence)` but the publisher's actual terminal state is
        // `PendingModeration` because the push was skipped. Dispatch
        // must surface that on the per-publisher row instead of the
        // default `Succeeded` mapping, or the summary table silently
        // misreports the skip as success.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake_with_pending_outcome(
            "chocolatey",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::PendingModeration,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let result = report
            .results
            .iter()
            .find(|r| r.name == "chocolatey")
            .expect("chocolatey entry present");
        assert!(
            matches!(result.outcome, PublisherOutcome::PendingModeration),
            "expected PendingModeration, got {:?}",
            result.outcome
        );
        assert!(
            result.evidence.is_some(),
            "evidence from run() should still be recorded alongside the override"
        );
    }

    #[test]
    fn dispatch_records_pending_validation_when_run_recorded_override() {
        // Mirrors winget/krew/homebrew-cask's PR-already-exists skip
        // path: `run()` returns `Ok(evidence)` but the actual terminal
        // state is `PendingValidation` because the PR could not be
        // created or updated. Dispatch must surface that override.
        let mut ctx = Context::test_fixture();
        let publishers = vec![fake_with_pending_outcome(
            "winget",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::PendingValidation,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let result = report
            .results
            .iter()
            .find(|r| r.name == "winget")
            .expect("winget entry present");
        assert!(
            matches!(result.outcome, PublisherOutcome::PendingValidation),
            "expected PendingValidation, got {:?}",
            result.outcome
        );
    }

    #[test]
    fn dispatch_records_full_anyhow_chain_on_failure() {
        // The user-visible failure message must surface every wrapped
        // `anyhow::Context` layer, not just the outermost one. Using
        // `Display` (the default `err.to_string()`) drops every cause,
        // which was the root cause of the mcp publisher's HTTP-status-less
        // "publish to https://..." log line — the inner HTTP 422 body was
        // discarded before reaching the operator. The recorded message
        // must contain BOTH the outer and inner segments, exactly as
        // `format!("{err:#}")` produces them.
        //
        // Parametrized across every `PublisherGroup` variant so a future
        // refactor that branches the closure per group cannot regress
        // one path silently. Assets/Manager/Submitter all flow through
        // the same `match p.run(ctx)` arm today — keep them all locked
        // in.
        use anodizer_core::PublishEvidence;
        use anyhow::Context as _;

        struct WrappedFailPublisher {
            name: &'static str,
            group: PublisherGroup,
        }
        impl Publisher for WrappedFailPublisher {
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
                Err::<(), _>(anyhow::anyhow!("inner"))
                    .context("middle")
                    .context("outer")?;
                unreachable!()
            }
            fn rollback(
                &self,
                _ctx: &mut Context,
                _evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        for (group, label) in [
            (PublisherGroup::Assets, "assets"),
            (PublisherGroup::Manager, "manager"),
            (PublisherGroup::Submitter, "submitter"),
        ] {
            let mut ctx = Context::test_fixture();
            let publishers: Vec<Box<dyn Publisher>> =
                vec![Box::new(WrappedFailPublisher { name: label, group })];
            let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
                .expect("dispatch returns Ok");

            let entry = report
                .results
                .iter()
                .find(|r| r.name == label)
                .unwrap_or_else(|| panic!("{label} entry present"));
            match &entry.outcome {
                PublisherOutcome::Failed(msg) => {
                    assert!(
                        msg.contains("outer"),
                        "{label}: outermost context must be present, got: {msg}"
                    );
                    assert!(
                        msg.contains("inner"),
                        "{label}: innermost cause must be present (anyhow `{{:#}}` chain), got: {msg}"
                    );
                }
                other => panic!("{label}: expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn dispatch_recovers_partial_evidence_from_failed_run() {
        // A publisher that did irreversible work for some items and then
        // failed stashes the partial evidence on the context before
        // returning `Err`. Dispatch must recover it into the failed row so
        // the rollback path has something to act on — without this, a
        // partial multi-crate cargo publish would leave the succeeded
        // crates live with no recorded yank target.
        use anodizer_core::PublishEvidence;

        struct PartialFailPublisher;
        impl Publisher for PartialFailPublisher {
            fn name(&self) -> &str {
                "partial"
            }
            fn group(&self) -> PublisherGroup {
                PublisherGroup::Submitter
            }
            fn required(&self) -> bool {
                true
            }
            fn skips_on_nightly(&self) -> bool {
                false
            }
            fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
                let mut ev = PublishEvidence::new("partial");
                ev.primary_ref = Some("https://crates.io/crates/crate-a/1.0.0".into());
                ctx.record_pending_evidence(ev);
                anyhow::bail!("crate-b failed after crate-a succeeded")
            }
            fn rollback(
                &self,
                _ctx: &mut Context,
                _evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn Publisher>> = vec![Box::new(PartialFailPublisher)];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let entry = report
            .results
            .iter()
            .find(|r| r.name == "partial")
            .expect("partial entry present");
        assert!(
            matches!(entry.outcome, PublisherOutcome::Failed(_)),
            "run failed, so the row is Failed: {:?}",
            entry.outcome
        );
        let ev = entry
            .evidence
            .as_ref()
            .expect("failed row must carry the partial evidence the run stashed");
        assert_eq!(
            ev.primary_ref.as_deref(),
            Some("https://crates.io/crates/crate-a/1.0.0"),
            "the partial evidence (crate-a) must survive into the report row"
        );
        // The slot is single-shot — drained by the recovery.
        assert!(
            ctx.take_pending_evidence().is_none(),
            "pending evidence must be consumed, not left to bleed forward"
        );
    }

    #[test]
    fn dispatch_drains_stale_override_between_publishers() {
        // The override slot is single-shot: an earlier publisher's
        // override must not bleed into a later publisher whose `run`
        // recorded nothing. Without the drain at the top of every
        // `run` invocation, a chocolatey moderation skip would
        // contaminate the next publisher's row.
        let mut ctx = Context::test_fixture();
        let publishers = vec![
            fake_with_pending_outcome(
                "first",
                PublisherGroup::Manager,
                false,
                PublisherOutcome::PendingModeration,
            ),
            fake(
                "second",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let first = report
            .results
            .iter()
            .find(|r| r.name == "first")
            .expect("first entry present");
        assert!(matches!(first.outcome, PublisherOutcome::PendingModeration));

        let second = report
            .results
            .iter()
            .find(|r| r.name == "second")
            .expect("second entry present");
        assert!(
            matches!(second.outcome, PublisherOutcome::Succeeded),
            "second publisher recorded no override; must default to Succeeded, got {:?}",
            second.outcome
        );
    }

    #[test]
    fn dispatch_substitutes_err_for_simulated_failure() {
        // Even when a publisher's `run()` would succeed, listing it in
        // `ctx.options.simulate_failure_publishers` must short-circuit
        // and record a synthetic `Failed("simulated failure: <name>")`
        // entry. A sibling that dispatches BEFORE the simulated failure runs
        // normally (brew is ordered first so the one-way-door gate is still
        // open when it dispatches) — keeping this test focused on the
        // substitution mechanic, not the gate (whose Manager coverage is
        // pinned by `required_manager_failure_gates_later_manager_oneway_door`).
        let mut ctx = Context::test_fixture();
        ctx.options.simulate_failure_publishers = vec!["cargo".to_string()];
        let publishers = vec![
            fake("brew", PublisherGroup::Manager, false, FakeOutcome::Succeed),
            fake("cargo", PublisherGroup::Manager, true, FakeOutcome::Succeed),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo entry present");
        match &cargo.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(
                    msg.contains("simulated failure") && msg.contains("cargo"),
                    "simulated-failure message should name the publisher, got: {}",
                    msg
                );
            }
            other => panic!("expected Failed, got {:?}", other),
        }
        assert!(
            cargo.evidence.is_none(),
            "simulated failure has no evidence"
        );

        let brew = report
            .results
            .iter()
            .find(|r| r.name == "brew")
            .expect("brew entry present");
        assert!(
            matches!(brew.outcome, PublisherOutcome::Succeeded),
            "non-simulated sibling publisher must run normally"
        );

        // Required-cargo failed + default gate_submitter => Submitter
        // group (if present) would be gated. Even without submitters
        // here the report should mark `any_failed(Manager, true)`.
        assert!(report.any_failed(PublisherGroup::Manager, true));
    }

    #[test]
    fn dispatch_skips_publisher_marked_skips_on_nightly_when_nightly() {
        let mut ctx = Context::test_fixture();
        ctx.options.nightly = true;
        let publishers = vec![
            fake_with_nightly_skip(
                "skipper",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "runner",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let skipper = report
            .results
            .iter()
            .find(|r| r.name == "skipper")
            .expect("skipper entry present");
        assert!(
            matches!(
                skipper.outcome,
                PublisherOutcome::Skipped(SkipReason::Nightly)
            ),
            "expected Skipped(Nightly), got {:?}",
            skipper.outcome
        );
        assert!(skipper.evidence.is_none());
        let runner = report
            .results
            .iter()
            .find(|r| r.name == "runner")
            .expect("runner entry present");
        assert!(
            matches!(runner.outcome, PublisherOutcome::Succeeded),
            "runner should not be skipped; got {:?}",
            runner.outcome
        );
    }

    #[test]
    fn dispatch_runs_publisher_marked_skips_on_nightly_when_not_nightly() {
        let mut ctx = Context::test_fixture();
        // options.nightly defaults to false
        let publishers = vec![fake_with_nightly_skip(
            "would_skip_on_nightly",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Succeed,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        let entry = report
            .results
            .iter()
            .find(|r| r.name == "would_skip_on_nightly")
            .expect("entry present");
        assert!(
            matches!(entry.outcome, PublisherOutcome::Succeeded),
            "publisher with skips_on_nightly=true must still run when !is_nightly(); got {:?}",
            entry.outcome
        );
    }

    /// `--skip=npm` deselects the npm publisher at the dispatch chokepoint
    /// even though npm has no stage-skip token; cargo (unlisted) runs.
    #[test]
    fn deselected_by_skip_records_deselected_and_does_not_run() {
        let mut ctx = Context::test_fixture();
        ctx.options.skip_stages = vec!["npm".to_string()];
        // The npm fake is wired to FAIL: if the dispatch loop ever invoked
        // its `run()`, the outcome would be `Failed`, not
        // `Skipped(Deselected)` — so a `Skipped(Deselected)` assertion is
        // itself proof that `run()` was never called.
        let publishers = vec![
            fake(
                "npm",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("npm run() must never be invoked when deselected".into()),
            ),
            fake(
                "cargo",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let npm = report
            .results
            .iter()
            .find(|r| r.name == "npm")
            .expect("npm recorded in report");
        assert!(
            matches!(
                npm.outcome,
                PublisherOutcome::Skipped(SkipReason::Deselected)
            ),
            "npm must be Skipped(Deselected) (and thus never run), got {:?}",
            npm.outcome
        );
        assert!(npm.evidence.is_none());

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo recorded in report");
        assert!(
            matches!(cargo.outcome, PublisherOutcome::Succeeded),
            "cargo (not in --skip) must run, got {:?}",
            cargo.outcome
        );
    }

    /// A non-empty `--publishers` allowlist runs ONLY the listed publishers;
    /// everything else records `Skipped(Deselected)`.
    #[test]
    fn allowlist_includes_only_listed_publisher() {
        let mut ctx = Context::test_fixture();
        ctx.options.publisher_allowlist = vec!["cargo".to_string()];
        let publishers = vec![
            fake(
                "cargo",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
            // npm would FAIL if run; being absent from the allowlist it must
            // be Skipped(Deselected) and never invoked.
            fake(
                "npm",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("npm run() must never be invoked when not in allowlist".into()),
            ),
        ];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo recorded");
        assert!(
            matches!(cargo.outcome, PublisherOutcome::Succeeded),
            "cargo (allowlisted) must run, got {:?}",
            cargo.outcome
        );

        let npm = report
            .results
            .iter()
            .find(|r| r.name == "npm")
            .expect("npm recorded");
        assert!(
            matches!(
                npm.outcome,
                PublisherOutcome::Skipped(SkipReason::Deselected)
            ),
            "npm (not allowlisted) must be Skipped(Deselected) and never run, got {:?}",
            npm.outcome
        );
    }

    /// `--skip` always wins over `--publishers`: a publisher present in BOTH
    /// is deselected, not run.
    #[test]
    fn skip_wins_over_allowlist() {
        let mut ctx = Context::test_fixture();
        ctx.options.publisher_allowlist = vec!["cargo".to_string()];
        ctx.options.skip_stages = vec!["cargo".to_string()];
        let publishers = vec![fake(
            "cargo",
            PublisherGroup::Submitter,
            false,
            FakeOutcome::Fail("cargo run() must never be invoked when --skip wins".into()),
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        let cargo = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo recorded");
        assert!(
            matches!(
                cargo.outcome,
                PublisherOutcome::Skipped(SkipReason::Deselected)
            ),
            "cargo listed in both --skip and --publishers must be Skipped(Deselected), got {:?}",
            cargo.outcome
        );
    }

    /// The `Deselected` skip lands in `report.results` so the run summary
    /// counts it (never a silent drop).
    #[test]
    fn deselected_skip_is_recorded_in_report_results() {
        let mut ctx = Context::test_fixture();
        ctx.options.skip_stages = vec!["npm".to_string()];
        let publishers = vec![fake(
            "npm",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Succeed,
        )];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");
        assert_eq!(
            report.results.len(),
            1,
            "the Deselected skip must be a recorded result, not a silent drop"
        );
        assert!(matches!(
            report.results[0].outcome,
            PublisherOutcome::Skipped(SkipReason::Deselected)
        ));
    }

    /// A deselected publisher's `run()` is NOT invoked — proven with a
    /// `run()`-invocation counter that is orthogonal to the recorded
    /// outcome. The counting fake's `run()` increments `run_calls` on
    /// every call; after dispatching a deselected publisher the counter
    /// must read `0`. This is independent of the companion
    /// `FakeOutcome::Fail` tests (which infer non-invocation from the
    /// `Skipped(Deselected)` outcome): even if the dispatch loop were
    /// mis-wired to both run AND mislabel the outcome, this counter would
    /// still catch the stray `run()` call.
    #[test]
    fn deselected_publisher_run_not_invoked() {
        let mut ctx = Context::test_fixture();
        ctx.options.skip_stages = vec!["npm".to_string()];
        let (publisher, run_calls) = fake_counting_runs("npm", PublisherGroup::Manager, false);
        let publishers = vec![publisher];
        let report = dispatch(&publishers, &mut ctx, &DispatchOptions::default())
            .expect("dispatch returns Ok");

        assert_eq!(
            run_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "deselected npm's run() must never be invoked",
        );

        let npm = &report.results[0];
        assert!(
            matches!(
                npm.outcome,
                PublisherOutcome::Skipped(SkipReason::Deselected)
            ),
            "deselected npm must record Skipped(Deselected), got {:?}",
            npm.outcome
        );
        assert!(
            npm.evidence.is_none(),
            "a publisher that never ran has no evidence"
        );
    }
}
