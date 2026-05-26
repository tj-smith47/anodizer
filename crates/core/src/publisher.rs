//! Publisher trait + preflight result type.
//!
//! Defines the polymorphic interface that every publisher (cargo, homebrew,
//! scoop, chocolatey, nix, AUR, krew, winget, snapcraft, blob, release, ...)
//! implements. Lives in `anodizer-core` rather than `stage-publish` so that
//! `stage-blob`, `stage-release`, and `stage-snapcraft` can implement
//! `Publisher` without taking a circular dependency on `stage-publish`.

use crate::context::Context;
use crate::{PublishEvidence, PublisherGroup};

/// Outcome of a publisher's pre-flight self-check.
///
/// Each variant signals a different release-pipeline reaction:
///
/// * `Pass` — no concern detected; publishing may proceed.
/// * `Warning(msg)` — surface the message to the operator (and review log)
///   but do not block the publish. Use for soft signals like "remote
///   already has a tag at this version but contents match".
/// * `Blocker(msg)` — abort before the publish stage runs. Use for hard
///   prerequisites the publisher knows it cannot satisfy at runtime, e.g.
///   "homebrew tap repo not reachable", "winget-pkgs fork not configured".
///
/// Named `Pass` (not `Clean`) to avoid nominal collision with
/// [`crate::preflight::PublisherState::Clean`], which describes the
/// already-published state of a publisher rather than a self-check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightCheck {
    /// Publisher's pre-flight checks completed with no concerns.
    Pass,
    /// Publisher detected a non-blocking concern; surface it but continue.
    Warning(String),
    /// Publisher detected a blocking concern; abort before the publish stage.
    Blocker(String),
}

/// Publisher contract — one implementer per upstream registry / channel.
///
/// Required methods describe the publisher's identity, behavior, and how
/// it participates in [`PublisherGroup`]-based scheduling:
///
/// * [`Publisher::name`] — stable identifier used in logs, evidence, and
///   review findings (e.g. `"cargo"`, `"homebrew"`, `"winget"`).
/// * [`Publisher::run`] — perform the actual publish and emit a
///   [`PublishEvidence`] record describing what was sent upstream.
/// * [`Publisher::group`] — which [`PublisherGroup`] this publisher belongs
///   to; used by the publish stage to order and parallelize work.
/// * [`Publisher::required`] — whether a failure in this publisher should
///   fail the overall release.
///
/// Default-implemented hooks describe optional behavior:
///
/// * [`Publisher::rollback`] — best-effort undo of a successful publish.
///   The default is a no-op so publishers that target irreversible
///   registries (most of them) do not need to override.
/// * [`Publisher::preflight`] — fast self-check executed before any
///   publisher in the pipeline runs. Defaults to [`PreflightCheck::Pass`].
/// * [`Publisher::rollback_scope_needed`] — declare an opt-in OAuth /
///   token scope that rollback would require (e.g. `"delete_repo"` for
///   GitHub-fork-based publishers). Defaults to `None`. Surfaced by
///   the CLI when explaining why a rollback path is unavailable.
///
/// Implementations must be `Send + Sync` so the publish stage can fan out
/// across publisher groups in parallel. Wrap non-`Send` clients (Rc-based,
/// thread-local channels) behind an `Arc<Mutex<_>>` or move them inside
/// `run()`'s scope rather than holding them on `self`.
pub trait Publisher: Send + Sync {
    /// Stable, lowercase identifier for this publisher (e.g. `"cargo"`).
    fn name(&self) -> &str;

    /// Execute the publish and emit evidence describing what was sent.
    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence>;

    /// Scheduling group — controls ordering and parallelism in the publish stage.
    fn group(&self) -> PublisherGroup;

    /// Whether a failure here should fail the overall release.
    fn required(&self) -> bool;

    /// Best-effort rollback of a successful publish, given its evidence.
    ///
    /// Default is a no-op: most upstream registries are append-only or
    /// require human moderation to revoke, so the publisher opts in by
    /// overriding only when it actually has a rollback path.
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        Ok(())
    }

    /// Fast self-check executed before any publisher runs.
    ///
    /// Default returns [`PreflightCheck::Pass`]. Override to surface
    /// publisher-specific blockers (missing tap, missing fork, network
    /// unreachable) or warnings (duplicate-but-matching upload).
    fn preflight(&self, _ctx: &Context) -> anyhow::Result<PreflightCheck> {
        Ok(PreflightCheck::Pass)
    }

    /// Opt-in OAuth / token scope rollback would require, if any.
    ///
    /// Default is `None`. Used by the CLI to explain why a `--rollback`
    /// invocation cannot recover a given publisher without elevating the
    /// release token's permissions.
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        None
    }

    /// Whether this publisher opts out of nightly runs (matches the GR
    /// `customization/publish/nightlies.md` skip-list).
    ///
    /// Default is `false`. Override to `true` for publishers that push to
    /// long-lived registries where a nightly clobber is either disruptive
    /// (homebrew taps, scoop buckets, AUR, krew-index, nix overlays) or
    /// outright forbidden by registry policy.
    fn skips_on_nightly(&self) -> bool {
        false
    }
}

/// The exact warn message a publisher emits when `rollback()` is invoked
/// with no evidence to act on (empty `artifact_paths`, no `primary_ref`).
/// Each publisher's empty-evidence branch calls this helper; tests can
/// assert on the returned string without having to intercept stderr
/// (`eprintln!` cannot be portably captured from the same process).
///
/// Lives in `anodizer_core` because the rollback shape is shared across
/// publishers spread between `stage-publish` and `stage-blob` (and any
/// future stage crate that implements `Publisher`).
pub fn rollback_empty_warning_msg(publisher: &str, target_label: &str) -> String {
    format!(
        "{}: no {} recorded in evidence; verify {} state manually",
        publisher, target_label, publisher
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MinimalPublisher;
    impl Publisher for MinimalPublisher {
        fn name(&self) -> &str {
            "minimal"
        }
        fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
            Ok(PublishEvidence::new("minimal"))
        }
        fn group(&self) -> PublisherGroup {
            PublisherGroup::Manager
        }
        fn required(&self) -> bool {
            false
        }
    }

    #[test]
    fn rollback_default_is_noop_ok() {
        let p = MinimalPublisher;
        let mut ctx = Context::test_fixture();
        let evidence = PublishEvidence::new("minimal");
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
    }

    #[test]
    fn preflight_default_is_pass() {
        let p = MinimalPublisher;
        let ctx = Context::test_fixture();
        assert!(matches!(p.preflight(&ctx).unwrap(), PreflightCheck::Pass));
    }

    #[test]
    fn rollback_scope_needed_default_is_none() {
        let p = MinimalPublisher;
        assert!(p.rollback_scope_needed().is_none());
    }

    #[test]
    fn pending_outcome_round_trips_through_context() {
        // The slot is single-shot: write once, drain once, then empty.
        // Without single-shot semantics, a chocolatey moderation skip
        // would bleed into the next publisher's row at dispatch time.
        let mut ctx = Context::test_fixture();
        assert!(ctx.take_pending_outcome().is_none());

        ctx.record_publisher_outcome(crate::PublisherOutcome::PendingModeration);
        assert!(matches!(
            ctx.take_pending_outcome(),
            Some(crate::PublisherOutcome::PendingModeration)
        ));
        assert!(
            ctx.take_pending_outcome().is_none(),
            "slot must be empty after take"
        );

        // Overwrite semantics: last writer wins (no implicit accumulation).
        ctx.record_publisher_outcome(crate::PublisherOutcome::PendingModeration);
        ctx.record_publisher_outcome(crate::PublisherOutcome::PendingValidation);
        assert!(matches!(
            ctx.take_pending_outcome(),
            Some(crate::PublisherOutcome::PendingValidation)
        ));
    }
}
