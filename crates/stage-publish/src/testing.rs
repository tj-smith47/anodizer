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
