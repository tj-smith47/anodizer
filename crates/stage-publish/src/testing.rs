//! Shared test doubles for publisher dispatch tests.
//!
//! Gated as `#[cfg(any(test, feature = "test-support"))] #[doc(hidden)]
//! pub mod testing;` in `lib.rs`. The `test-support` Cargo feature is
//! enabled by this crate's own `[dev-dependencies]` so integration tests
//! under `tests/` can import the same fakes the in-crate unit tests
//! use. NOT a stable public API.

use anodizer_core::context::Context;
use anodizer_core::{PublishEvidence, Publisher, PublisherGroup, PublisherOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Drives [`FakePublisher::run`].
pub enum FakeOutcome {
    Succeed,
    Fail(String),
}

/// Drives [`FakePublisher::rollback`]. Independent from [`FakeOutcome`]
/// because publishing and rollback are exercised by separate tests:
/// rollback dispatch only walks publishers whose `run()` succeeded, so
/// the rollback-failure path needs `Succeed` for the publish side AND a
/// failing rollback to verify the per-step `RollbackFailed` outcome.
pub enum FakeRollback {
    Succeed,
    Fail(String),
}

/// Minimal [`Publisher`] implementation that records its identity and
/// returns a predetermined [`FakeOutcome`] from `run`.
pub struct FakePublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub outcome: FakeOutcome,
    pub rollback_outcome: FakeRollback,
    /// Mirrors [`Publisher::rollback_scope_needed`]. When `Some`, the
    /// rollback dispatcher checks for the corresponding env var before
    /// invoking `rollback()`.
    pub rollback_scope: Option<&'static str>,
}

impl Publisher for FakePublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        match &self.outcome {
            FakeOutcome::Succeed => Ok(PublishEvidence::new(self.name.clone())),
            FakeOutcome::Fail(msg) => anyhow::bail!("{}", msg),
        }
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        match &self.rollback_outcome {
            FakeRollback::Succeed => Ok(()),
            FakeRollback::Fail(msg) => anyhow::bail!("{}", msg),
        }
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        self.rollback_scope
    }
}

/// Convenience constructor returning the boxed-trait-object shape the
/// dispatcher consumes. Defaults rollback to a no-op success and
/// declares no scope, matching the dispatcher's "rollback runs cleanly"
/// case used by every dispatch-side test.
pub fn fake(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome: FakeRollback::Succeed,
        rollback_scope: None,
    })
}

/// Like [`fake`] but lets the test drive both the publish outcome AND
/// the rollback outcome. Use for rollback-failure tests where the
/// publisher must `Succeed` (so rollback dispatch picks it up) but the
/// `rollback()` call itself returns `Err`.
pub fn fake_with_rollback(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
    rollback_outcome: FakeRollback,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome,
        rollback_scope: None,
    })
}

/// Minimal [`Publisher`] that counts its `rollback()` invocations.
/// Used by idempotency tests that need to assert "rollback() was NOT
/// called a second time" across two `run_with_publishers` invocations
/// — the standard [`FakePublisher`] exposes no such counter.
pub struct FakeCountingPublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub rollback_calls: Arc<AtomicUsize>,
}

impl Publisher for FakeCountingPublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        Ok(PublishEvidence::new(self.name.clone()))
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        self.rollback_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Convenience constructor for [`FakeCountingPublisher`]. Returns the
/// boxed publisher alongside the shared counter so the test can assert
/// `counter.load(Ordering::SeqCst) == N` after dispatch.
pub fn fake_counting(
    name: &str,
    group: PublisherGroup,
    required: bool,
) -> (Box<dyn Publisher>, Arc<AtomicUsize>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let publisher = Box::new(FakeCountingPublisher {
        name: name.to_string(),
        group,
        required,
        rollback_calls: counter.clone(),
    });
    (publisher, counter)
}

/// Minimal [`Publisher`] whose `run()` returns `Ok` but records an
/// override [`PublisherOutcome`] on the context via
/// [`Context::record_publisher_outcome`]. Used by dispatch-level tests
/// that verify the override is respected (instead of the default
/// `Succeeded` mapping) — mirrors how chocolatey's moderation skip
/// and winget/krew/homebrew-cask's PR-already-exists skip report
/// `PendingModeration` / `PendingValidation` from a successful `run`.
pub struct FakeOutcomePublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub pending_outcome: PublisherOutcome,
}

impl Publisher for FakeOutcomePublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        ctx.record_publisher_outcome(self.pending_outcome.clone());
        Ok(PublishEvidence::new(self.name.clone()))
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Convenience constructor for [`FakeOutcomePublisher`].
pub fn fake_with_pending_outcome(
    name: &str,
    group: PublisherGroup,
    required: bool,
    pending_outcome: PublisherOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakeOutcomePublisher {
        name: name.to_string(),
        group,
        required,
        pending_outcome,
    })
}

/// Like [`fake`] but declares a non-`None` `rollback_scope_needed`. Use
/// for the `RollbackSkippedNoScope` path where the dispatcher should
/// skip the rollback because the env var is unset.
pub fn fake_with_scope(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
    rollback_scope: &'static str,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome: FakeRollback::Succeed,
        rollback_scope: Some(rollback_scope),
    })
}

/// Shared behavioral contract for per-crate publishers — catches the
/// silent-success class of bug where a publisher's `run()` returns `Ok`
/// with no operator-visible signal, indistinguishable from a real push
/// in the dispatch table.
///
/// The contract asserts the following hold after `publisher.run(&mut ctx)`
/// returns:
///
/// 1. `run()` returns `Ok(evidence)` and `evidence.publisher` matches
///    `publisher.name()`.
/// 2. At least three `status` log lines were emitted — the standard
///    start + per-crate-start + done pattern every per-crate publisher
///    uses. This is the load-bearing assertion that the publisher
///    actually visited its crates rather than silently `continue`-ing
///    through every iteration.
/// 3. Either:
///    - `evidence` is non-empty (`primary_ref` set or `extra` object
///      non-empty), **OR**
///    - the publisher emitted at least one `warn` line (e.g. the
///      no-eligible-crates remediation path fires), **OR**
///    - at least one extra `status` line beyond the standard three
///      was emitted — covering the dry-run / skip path where
///      publishers explicitly log `(dry-run) would push ...` instead
///      of recording rollback evidence. Without this branch the
///      contract would conflate "no evidence in dry-run" (correct;
///      avoids phantom rollback targets) with "silent success" (bug).
///
/// The capture is attached via [`Context::with_log_capture`] so every
/// logger built inside `publisher.run` records to the same vec without
/// any stderr-intercept gymnastics.
///
/// # Arguments
///
/// * `publisher` — the publisher under test.
/// * `ctx` — a context the caller built so the publisher's
///   configured-crate predicate selects at least one crate (typically
///   via [`anodizer_core::test_helpers::TestContextBuilder`] with a
///   single appropriately-configured crate and `dry_run(true)`).
///
/// # Panics
///
/// Panics (failing the test) if any contract invariant is violated.
pub fn assert_publisher_visible_work_contract(publisher: &dyn Publisher, ctx: &mut Context) {
    use anodizer_core::log::LogCapture;

    let capture = LogCapture::new();
    ctx.with_log_capture(capture.clone());

    let evidence = publisher
        .run(ctx)
        .unwrap_or_else(|e| panic!("publisher '{}' run() failed: {e:#}", publisher.name()));

    assert_eq!(
        evidence.publisher,
        publisher.name(),
        "evidence.publisher must match publisher.name()"
    );

    let status_count = capture.status_count();
    let warn_count = capture.warn_count();
    // Typed enum: `Empty` is the only "no operator-public fields"
    // variant. Anything else carries at least one target struct.
    let extra_is_empty = matches!(evidence.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let has_evidence = evidence.primary_ref.is_some() || !extra_is_empty;

    assert!(
        status_count >= 3,
        "publisher '{}': expected ≥3 status log lines (start + per-crate-start + done), got {}. \
         Captured: {:?}",
        publisher.name(),
        status_count,
        capture.all_messages()
    );

    // A publisher satisfies the visible-work contract when ANY of these
    // operator-readable signals is present:
    //   - evidence has content (production push completed), OR
    //   - a warn fired (no-eligible-crates explanation), OR
    //   - the loop emitted more than the bare start/per-crate-start/done
    //     trio — i.e. at least one dry-run / skip / per-crate-progress
    //     status line, which is what tells the operator what would have
    //     happened on a real push.
    let extra_status_lines = status_count > 3;
    assert!(
        has_evidence || warn_count >= 1 || extra_status_lines,
        "publisher '{}': run() returned empty evidence AND emitted zero warnings AND \
         emitted no progress status beyond the standard start/per-crate-start/done trio \
         — operator has no signal this publisher did anything. Captured: {:?}",
        publisher.name(),
        capture.all_messages()
    );
}
