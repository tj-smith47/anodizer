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

impl PreflightCheck {
    /// Fold two pre-flight outcomes into the most severe, escalating
    /// `Blocker` > `Warning` > `Pass`. Within a severity the first-seen
    /// message (`self`'s) wins, so a left-fold over many targets yields a
    /// stable, deterministic line rather than whichever target iterated last.
    pub fn merge(self, next: Self) -> Self {
        use PreflightCheck::{Blocker, Pass, Warning};
        match (self, next) {
            (Blocker(m), _) => Blocker(m),
            (_, Blocker(m)) => Blocker(m),
            (Warning(m), _) => Warning(m),
            (_, Warning(m)) => Warning(m),
            (Pass, Pass) => Pass,
        }
    }
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

    /// Environment requirements this publisher derives from the resolved
    /// config: CLI tools it spawns, env vars/secrets it reads, endpoints
    /// it talks to, key material it loads.
    ///
    /// Consumed by the config-aware preflight (`anodizer preflight` and the
    /// in-process phase at the head of `anodizer release`). Declared next
    /// to each publisher's implementation — derived from the same config
    /// fields `run()` reads — so the preflight cannot drift from the
    /// publish path. Default is empty for publishers with no external
    /// prerequisites beyond what their stage already declares.
    fn requirements(&self, _ctx: &Context) -> Vec<crate::env_preflight::EnvRequirement> {
        Vec::new()
    }

    /// Environment requirements whose absence DEGRADES this publisher's run
    /// rather than failing it: optional validators (`ruby -c`, `bash -n`,
    /// `nix-instantiate --parse`) that warn+skip when missing, or a preferred
    /// transport with a full fallback (`gh` vs the GitHub REST API).
    ///
    /// Collected alongside [`Publisher::requirements`] but surfaced as
    /// ADVISORY: preflight warns instead of blocking, and `anodizer tools`
    /// reports them as recommended so an auto-provisioned runner installs
    /// them and gets the stronger validation/transport. Hard needs (the run
    /// path errors without the tool) belong in `requirements()` instead.
    /// Default is empty.
    fn advisory_requirements(&self, _ctx: &Context) -> Vec<crate::env_preflight::EnvRequirement> {
        Vec::new()
    }

    /// True when this publisher was registered (a config block exists) but
    /// every configured entry evaluates skip-inactive under the CURRENT
    /// config/env — `skip:`/`skip_upload:` truthy or `if:` falsy on all of
    /// them. Checked at the dispatch chokepoint BEFORE [`Publisher::run`]
    /// runs, so a `run()` that unconditionally returns `Ok(evidence)` even
    /// with zero active entries is never recorded as `Succeeded`.
    ///
    /// Default `false`: publishers with no skip/enable knob need no
    /// override. Publishers that do have one implement this by reusing the
    /// exact active-entries predicate their [`Publisher::requirements`]
    /// already applies — never a second, independently-derived skip check —
    /// so the two cannot drift.
    fn config_fully_inactive(&self, _ctx: &Context) -> bool {
        false
    }

    /// Whether this publisher opts out of nightly runs (the
    /// `customization/publish/nightlies.md` skip-list).
    ///
    /// Each `Publisher` must declare its nightly behavior explicitly — there
    /// is no default — so adding a new publisher forces a deliberate decision.
    /// Return `true` for publishers that push to long-lived registries where a
    /// nightly clobber is either disruptive (homebrew taps, scoop buckets,
    /// AUR, krew-index, nix overlays) or outright forbidden by registry policy.
    fn skips_on_nightly(&self) -> bool;

    /// Whether a *failed* run of this publisher still has a real,
    /// programmatic rollback to perform against `evidence`.
    ///
    /// Default `false`: a publisher's failure leaves nothing to undo (or
    /// only an informational, human-driven unwind). The orchestration
    /// rolls back **succeeded** Assets/Manager publishers; a failed
    /// Submitter is normally inert.
    ///
    /// The cargo publisher is the exception. A multi-crate `cargo publish`
    /// can succeed on crate A, go live on crates.io, then fail on crate B
    /// — leaving A published under a *failed* Submitter row. cargo records
    /// the succeeded crates in `evidence` and overrides this to `true`
    /// when that set is non-empty, so the rollback path yanks A even
    /// though the publisher's overall outcome is `Failed`. Returning
    /// `false` for an empty record keeps a clean failure (nothing went
    /// live) from arming the rollback machinery for no reason.
    fn programmatic_rollback_on_failure(&self, _evidence: &PublishEvidence) -> bool {
        false
    }

    /// When `true`, this publisher's successful work is left in place even
    /// when a rollback is triggered — it is never passed to `rollback()`.
    /// Default `false` (rollback runs if the publisher implements it).
    fn retain_on_rollback(&self) -> bool {
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
        "no {} recorded in {} evidence — verify {} state manually",
        target_label, publisher, publisher
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
        fn skips_on_nightly(&self) -> bool {
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

    #[test]
    fn rollback_empty_warning_msg_interpolates_all_three_slots() {
        let msg = rollback_empty_warning_msg("homebrew", "tap commit");
        assert_eq!(
            msg,
            "no tap commit recorded in homebrew evidence — verify homebrew state manually"
        );
    }

    #[test]
    fn rollback_empty_warning_msg_distinct_per_publisher() {
        let a = rollback_empty_warning_msg("cargo", "crate");
        let b = rollback_empty_warning_msg("aur", "commit");
        assert_ne!(a, b);
        assert!(a.contains("cargo") && a.contains("crate"));
        assert!(b.contains("aur") && b.contains("commit"));
    }

    #[test]
    fn programmatic_rollback_on_failure_defaults_false() {
        let p = MinimalPublisher;
        let evidence = PublishEvidence::new("minimal");
        assert!(!p.programmatic_rollback_on_failure(&evidence));
    }

    #[test]
    fn retain_on_rollback_defaults_false() {
        assert!(!MinimalPublisher.retain_on_rollback());
    }

    #[test]
    fn requirements_default_is_empty() {
        let p = MinimalPublisher;
        let ctx = Context::test_fixture();
        assert!(p.requirements(&ctx).is_empty());
    }

    #[test]
    fn config_fully_inactive_defaults_false() {
        let p = MinimalPublisher;
        let ctx = Context::test_fixture();
        assert!(!p.config_fully_inactive(&ctx));
    }

    #[test]
    fn preflight_check_variants_compare_by_value() {
        assert_eq!(PreflightCheck::Pass, PreflightCheck::Pass);
        assert_eq!(
            PreflightCheck::Warning("dup".into()),
            PreflightCheck::Warning("dup".into())
        );
        // same variant, different payload, must not be equal
        assert_ne!(
            PreflightCheck::Blocker("a".into()),
            PreflightCheck::Blocker("b".into())
        );
        // different variants with same string must not be equal
        assert_ne!(
            PreflightCheck::Warning("x".into()),
            PreflightCheck::Blocker("x".into())
        );
    }

    #[test]
    fn minimal_publisher_carries_its_declared_identity() {
        let p = MinimalPublisher;
        assert_eq!(p.name(), "minimal");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert!(!p.skips_on_nightly());
    }

    /// A publisher that overrides every default-implemented hook, so the
    /// trait dispatch is proven to reach the override (not silently shadowed
    /// by the default body).
    struct OverridingPublisher;
    impl Publisher for OverridingPublisher {
        fn name(&self) -> &str {
            "overriding"
        }
        fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
            Ok(PublishEvidence::new("overriding"))
        }
        fn group(&self) -> PublisherGroup {
            PublisherGroup::Assets
        }
        fn required(&self) -> bool {
            true
        }
        fn skips_on_nightly(&self) -> bool {
            true
        }
        fn preflight(&self, _ctx: &Context) -> anyhow::Result<PreflightCheck> {
            Ok(PreflightCheck::Blocker("fork missing".into()))
        }
        fn rollback_scope_needed(&self) -> Option<&'static str> {
            Some("delete_repo")
        }
        fn programmatic_rollback_on_failure(&self, _evidence: &PublishEvidence) -> bool {
            true
        }
        fn retain_on_rollback(&self) -> bool {
            true
        }
    }

    #[test]
    fn override_publisher_preflight_returns_blocker() {
        let p = OverridingPublisher;
        let ctx = Context::test_fixture();
        assert_eq!(
            p.preflight(&ctx).unwrap(),
            PreflightCheck::Blocker("fork missing".into())
        );
    }

    #[test]
    fn override_publisher_exposes_rollback_scope_and_flags() {
        let p = OverridingPublisher;
        let evidence = PublishEvidence::new("overriding");
        assert_eq!(p.rollback_scope_needed(), Some("delete_repo"));
        assert!(p.programmatic_rollback_on_failure(&evidence));
        assert!(p.retain_on_rollback());
        assert!(p.required());
        assert!(p.skips_on_nightly());
        assert_eq!(p.group(), PublisherGroup::Assets);
    }

    #[test]
    fn preflight_check_clone_preserves_payload() {
        let warn = PreflightCheck::Warning("dup upload".into());
        assert_eq!(warn.clone(), warn);
        let blocker = PreflightCheck::Blocker("no tap".into());
        let cloned = blocker.clone();
        assert_eq!(cloned, PreflightCheck::Blocker("no tap".into()));
        // Clone must not collapse a Warning into the same value as a Blocker.
        assert_ne!(warn, blocker);
    }

    #[test]
    fn merge_escalates_to_worst_severity_keeping_first_message() {
        use PreflightCheck::{Blocker, Pass, Warning};

        // Blocker dominates regardless of position, keeping its own message.
        assert_eq!(
            Blocker("b".into()).merge(Warning("w".into())),
            Blocker("b".into())
        );
        assert_eq!(
            Warning("w".into()).merge(Blocker("b".into())),
            Blocker("b".into())
        );
        assert_eq!(Pass.merge(Blocker("b".into())), Blocker("b".into()));
        assert_eq!(Blocker("b".into()).merge(Pass), Blocker("b".into()));

        // Warning dominates Pass.
        assert_eq!(Warning("w".into()).merge(Pass), Warning("w".into()));
        assert_eq!(Pass.merge(Warning("w".into())), Warning("w".into()));

        // Pass + Pass stays Pass.
        assert_eq!(Pass.merge(Pass), Pass);

        // Within a severity the first-seen (left) message wins.
        assert_eq!(
            Blocker("first".into()).merge(Blocker("second".into())),
            Blocker("first".into())
        );
        assert_eq!(
            Warning("first".into()).merge(Warning("second".into())),
            Warning("first".into())
        );
    }
}
