//! Cross-publisher artifact promotion.
//!
//! Promotion moves an already-published artifact from a pre-release track to a
//! more stable track **without rebuilding** — a snapcraft channel release, an
//! npm dist-tag move, an OCI floating-tag re-point, or a GitHub prerelease
//! flip. It is a cross-publisher capability, not snap-specific: four publishers
//! own the mechanic and each speaks its own track vocabulary.
//!
//! This module defines the publisher-agnostic surface the `anodizer promote`
//! verb fans out over:
//!
//! * [`Promotable`] — the capability a promotion-capable publisher implements.
//! * [`PromoteRequest`] — the resolved (native) `from`/`to` tracks, the
//!   [`PromoteSelector`], the dry-run flag, and the [`Context`] handle a
//!   publisher reads its config, version, and logger from.
//! * [`PromoteOutcome`] / [`PromoteReport`] — the per-publisher result the verb
//!   renders a summary from, parallel to
//!   [`crate::publish_report::PublisherResult`] /
//!   [`crate::publish_report::PublishReport`] so a promotion run reports the
//!   same shape a publish run does.
//!
//! ## Why a trait (and not the stage/report dispatch the publish path uses)
//!
//! The publish path splits its dispatch: trait-based [`Publisher`]s run through
//! a central registry, while snapcraft runs as its own [`crate::stage::Stage`]
//! (a trait registration would double-publish). Promotion has no such hazard —
//! there is exactly one promotion action per publisher and no gate ordering — so
//! a single [`Promotable`] trait object per capable publisher is the cleanest
//! fit. The trait lives in `anodizer-core` (like [`Publisher`]) so each
//! publisher's own stage crate can implement it without a circular dependency,
//! and so the subprocess spawn a promotion needs (e.g. `snapcraft release`)
//! stays inside the module-boundary allow-list of that stage crate rather than
//! leaking into `core` or the CLI. The `anodizer promote` verb assembles the
//! `Vec<Box<dyn Promotable>>` (it is the one crate that depends on every stage)
//! and hands it to [`dispatch_promotions`]; adding npm / docker / github
//! promotion is a new `impl Promotable` plus one line in that assembly.
//!
//! [`Publisher`]: crate::publisher::Publisher

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::publish_report::PublishReport;

/// Canonical, publisher-neutral track names the CLI accepts for `--from` /
/// `--to`. `stable` is the promotion target; the rest are pre-stable aliases
/// each publisher's [`Promotable::resolve_track`] maps into its native
/// vocabulary. A raw native name (e.g. a snapcraft `edge` or an npm `next`
/// dist-tag) that is not in this set passes through `resolve_track` verbatim,
/// so operators are never boxed into the canonical words.
pub const CANONICAL_TRACKS: &[&str] = &["stable", "prerelease", "candidate", "beta", "edge"];

/// The canonical pre-stable aliases (every CANONICAL_TRACKS entry except
/// `stable`). A publisher with a single native pre-track maps all of these to
/// that track; snapcraft (real edge/beta/candidate channels) maps them
/// individually and is the deliberate exception.
pub const CANONICAL_PRETRACKS: &[&str] = &["prerelease", "candidate", "beta", "edge"];

/// Whether `name` is a canonical pre-stable alias (see [`CANONICAL_PRETRACKS`]).
pub fn is_canonical_pretrack(name: &str) -> bool {
    CANONICAL_PRETRACKS.contains(&name)
}

/// Publisher-neutral default for `--from` when the operator omits it: the
/// pre-stable track, whatever the selected publisher calls it. Each publisher's
/// [`Promotable::resolve_track`] maps `"prerelease"` into its native pre-stable
/// track (snapcraft → `candidate`).
pub const DEFAULT_FROM_TRACK: &str = "prerelease";

/// Identifiers of the publishers that currently implement [`Promotable`]. The
/// `anodizer promote` verb consults this to distinguish "named a publisher that
/// cannot be promoted" (a clear error) from "named a promotable publisher that
/// is not configured". It lists only publishers with a real implementation so
/// the error messages never claim a capability that does not exist yet.
pub const PROMOTABLE_PUBLISHERS: &[&str] = &["snapcraft", "npm", "docker", "github"];

/// Whether `name` identifies a promotion-capable publisher (see
/// [`PROMOTABLE_PUBLISHERS`]).
pub fn is_promotion_capable(name: &str) -> bool {
    PROMOTABLE_PUBLISHERS.contains(&name)
}

/// Which already-published artifact a promotion should move.
///
/// Carried by [`PromoteRequest`] and interpreted by each publisher against its
/// own native coordinates (a snapcraft revision, an npm version, an OCI digest,
/// a GitHub tag).
#[derive(Debug, Clone)]
pub enum PromoteSelector {
    /// Promote the newest artifact currently landed in the `from` track
    /// (publisher-resolved — e.g. the highest snapcraft revision in the
    /// from-channel).
    Newest,
    /// Promote this explicit version / tag.
    Version(String),
    /// Promote what a prior release run recorded. Carries the loaded
    /// [`PublishReport`] so publishers read the recorded coordinates
    /// (snapcraft revision, npm version, …) straight from the evidence instead
    /// of re-parsing run-summary files. The `run_id` is retained for
    /// diagnostics and dry-run rendering.
    FromRun {
        /// The run id the report was loaded from (`--from-run <id>`).
        run_id: String,
        /// The prior run's `report.json`, already parsed.
        report: PublishReport,
    },
}

impl PromoteSelector {
    /// A short operator-facing description of what this selector targets, used
    /// in dry-run and summary lines (e.g. `version 1.2.3`, `newest`,
    /// `run abc123`).
    pub fn describe(&self) -> String {
        match self {
            PromoteSelector::Newest => "newest".to_string(),
            PromoteSelector::Version(v) => format!("version {v}"),
            PromoteSelector::FromRun { run_id, .. } => format!("run {run_id}"),
        }
    }

    /// The `from` label a folded promotion summary/outcome should show. An
    /// explicit selector names the source it actually targets (a concrete
    /// version, a recorded run), so the summary reads `1.4.0→latest` rather
    /// than the canonical track direction `edge→latest` that would mislead when
    /// the operator passed `--version`/`--from-run`. `Newest` keeps the native
    /// `from_track` (it genuinely promotes whatever sits on that track).
    pub fn source_label(&self, from_track: &str) -> String {
        match self {
            PromoteSelector::Newest => from_track.to_string(),
            PromoteSelector::Version(v) => v.clone(),
            PromoteSelector::FromRun { run_id, .. } => format!("run {run_id}"),
        }
    }
}

/// A single publisher's promotion request: the resolved **native** `from`/`to`
/// tracks (already mapped through [`Promotable::resolve_track`] by
/// [`dispatch_promotions`]), the [`PromoteSelector`], the dry-run flag, and the
/// [`Context`] handle the publisher reads config / version / logger from.
pub struct PromoteRequest<'a> {
    /// Source track, in this publisher's native vocabulary.
    pub from: String,
    /// Destination track, in this publisher's native vocabulary.
    pub to: String,
    /// What to promote.
    pub selector: &'a PromoteSelector,
    /// When true, resolve and print the plan but run no external command.
    pub dry_run: bool,
    /// Shared context: config, resolved version, logger.
    pub ctx: &'a Context,
}

/// Why a publisher's promotion did not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromoteSkipReason {
    /// The publisher does not support promotion (defensive — the verb filters
    /// these out before dispatch; recorded if a caller dispatches one anyway).
    Unsupported,
    /// The publisher is promotion-capable but had nothing to promote in the
    /// `from` track (e.g. no matching snapcraft revision).
    NothingToPromote,
}

/// Terminal state of a single publisher's promotion, parallel to
/// [`crate::publish_report::PublisherOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromoteStatus {
    /// The artifact was moved from `from` to `to`.
    Promoted,
    /// `--dry-run`: the plan was printed; nothing was moved.
    DryRun,
    /// The promotion did not run; see [`PromoteSkipReason`].
    Skipped(PromoteSkipReason),
    /// The promotion was attempted and failed; the `String` is the rendered
    /// error (`{:#}`).
    Failed(String),
}

/// Per-publisher promotion result, parallel to
/// [`crate::publish_report::PublisherResult`]. Rendered by the verb into a
/// summary line and folded into the process exit code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteOutcome {
    /// Publisher identifier (e.g. `"snapcraft"`).
    pub publisher: String,
    /// Source track, native vocabulary.
    pub from: String,
    /// Destination track, native vocabulary.
    pub to: String,
    /// The coordinate that was promoted (revision / version / tag / digest),
    /// when known. `None` for dry-run and skip outcomes.
    pub what: Option<String>,
    /// Terminal state.
    pub status: PromoteStatus,
}

impl PromoteOutcome {
    /// Build a `Promoted` outcome naming the coordinate that moved.
    pub fn promoted(
        publisher: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        what: impl Into<String>,
    ) -> Self {
        Self {
            publisher: publisher.into(),
            from: from.into(),
            to: to.into(),
            what: Some(what.into()),
            status: PromoteStatus::Promoted,
        }
    }

    /// Build a `DryRun` outcome; `what` is the resolved coordinate when the
    /// publisher could name it without spawning, else `None`.
    pub fn dry_run(
        publisher: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        what: Option<String>,
    ) -> Self {
        Self {
            publisher: publisher.into(),
            from: from.into(),
            to: to.into(),
            what,
            status: PromoteStatus::DryRun,
        }
    }

    /// Build a `Skipped` outcome.
    pub fn skipped(
        publisher: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        reason: PromoteSkipReason,
    ) -> Self {
        Self {
            publisher: publisher.into(),
            from: from.into(),
            to: to.into(),
            what: None,
            status: PromoteStatus::Skipped(reason),
        }
    }

    /// Build a `Failed` outcome carrying the rendered error.
    pub fn failed(
        publisher: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            publisher: publisher.into(),
            from: from.into(),
            to: to.into(),
            what: None,
            status: PromoteStatus::Failed(error.into()),
        }
    }

    /// Whether this outcome is a terminal failure that must fail the verb.
    ///
    /// Exhaustive `match` (not `matches!`) so a future terminal-failure variant
    /// forces a conscious classification decision here, mirroring
    /// [`crate::publish_report::PublisherOutcome::is_required_release_failure`].
    pub fn is_failure(&self) -> bool {
        match self.status {
            PromoteStatus::Failed(_) => true,
            PromoteStatus::Promoted | PromoteStatus::DryRun | PromoteStatus::Skipped(_) => false,
        }
    }

    /// One-line operator-facing summary, e.g.
    /// `snapcraft: revision 42 candidate→stable (promoted)`.
    pub fn summary_line(&self) -> String {
        let coord = self
            .what
            .as_deref()
            .map(|w| format!("{w} "))
            .unwrap_or_default();
        let state = match &self.status {
            PromoteStatus::Promoted => "promoted".to_string(),
            PromoteStatus::DryRun => "dry-run".to_string(),
            PromoteStatus::Skipped(PromoteSkipReason::Unsupported) => {
                "skipped (unsupported)".into()
            }
            PromoteStatus::Skipped(PromoteSkipReason::NothingToPromote) => {
                "skipped (nothing to promote)".into()
            }
            PromoteStatus::Failed(msg) => format!("failed: {msg}"),
        };
        format!(
            "{}: {}{}→{} ({})",
            self.publisher, coord, self.from, self.to, state
        )
    }
}

/// Aggregate result of a promotion run, parallel to
/// [`crate::publish_report::PublishReport`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteReport {
    /// One entry per dispatched publisher, in dispatch order.
    pub results: Vec<PromoteOutcome>,
}

impl PromoteReport {
    /// Whether any dispatched publisher's promotion failed.
    pub fn any_failure(&self) -> bool {
        self.results.iter().any(PromoteOutcome::is_failure)
    }

    /// Names of the publishers whose promotion failed.
    pub fn failure_names(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| r.is_failure())
            .map(|r| r.publisher.as_str())
            .collect()
    }
}

/// Build the `bail!` message for a best-effort multi-target promotion in which
/// at least one sub-target failed. Names both what was already applied (those
/// successes stay in place — promotion is idempotent on re-run) and every
/// sub-target that failed, each with its rendered cause, so an operator sees the
/// partial state at a glance:
/// `promoted 2/3 (acme/app, acme/lib); failed on acme/tool: <cause>`.
pub fn partial_promotion_error(applied: &[String], failed: &[(String, String)]) -> String {
    let total = applied.len() + failed.len();
    let applied_list = if applied.is_empty() {
        "none".to_string()
    } else {
        applied.join(", ")
    };
    let failed_detail = failed
        .iter()
        .map(|(target, cause)| format!("{target}: {cause}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "promoted {}/{total} ({applied_list}); failed on {failed_detail}",
        applied.len()
    )
}

/// The capability a promotion-capable publisher implements.
///
/// Each implementer maps canonical track names into its own vocabulary
/// ([`resolve_track`](Promotable::resolve_track)) and moves an
/// already-published artifact between tracks ([`promote`](Promotable::promote)).
/// Implementations must be `Send + Sync` (matching [`Publisher`]) so the verb
/// can hold them behind trait objects.
///
/// [`Publisher`]: crate::publisher::Publisher
pub trait Promotable: Send + Sync {
    /// Stable, lowercase identifier (e.g. `"snapcraft"`) — matches the
    /// publisher's [`crate::publisher::Publisher::name`] so `--publishers`
    /// selection and summaries use one vocabulary.
    fn name(&self) -> &str;

    /// Map a canonical track name (see [`CANONICAL_TRACKS`]) into this
    /// publisher's native track vocabulary. An unrecognized name — including a
    /// raw native track the operator typed directly — passes through verbatim,
    /// so promotion is never restricted to the canonical words.
    fn resolve_track(&self, canonical: &str) -> String;

    /// Move the selected artifact from `req.from` to `req.to` (both already in
    /// this publisher's native vocabulary). Honors `req.dry_run` by resolving
    /// and printing the plan without running any external command.
    fn promote(&self, req: &PromoteRequest) -> anyhow::Result<PromoteOutcome>;
}

/// Fan out a promotion across `publishers`, resolving the canonical `from`/`to`
/// into each publisher's native vocabulary and collecting one
/// [`PromoteOutcome`] each. A publisher that returns `Err` is recorded as
/// [`PromoteStatus::Failed`] (never aborting the fan-out) so the report
/// enumerates every publisher's result, mirroring the publish dispatch's
/// per-publisher-failure discipline. `ctx.is_dry_run()` drives `req.dry_run`.
pub fn dispatch_promotions(
    publishers: &[Box<dyn Promotable>],
    canonical_from: &str,
    canonical_to: &str,
    selector: &PromoteSelector,
    ctx: &Context,
) -> PromoteReport {
    let dry_run = ctx.is_dry_run();
    let mut report = PromoteReport::default();
    for p in publishers {
        let from = p.resolve_track(canonical_from);
        let to = p.resolve_track(canonical_to);
        let req = PromoteRequest {
            from: from.clone(),
            to: to.clone(),
            selector,
            dry_run,
            ctx,
        };
        let outcome = match p.promote(&req) {
            Ok(o) => o,
            Err(err) => PromoteOutcome::failed(p.name(), from, to, format!("{err:#}")),
        };
        report.results.push(outcome);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake promotable that maps `prerelease`→`beta` and records requests,
    /// so the dispatcher's resolve-then-promote wiring can be asserted without
    /// a real publisher.
    struct FakePromotable {
        name: &'static str,
        behavior: FakeBehavior,
    }

    enum FakeBehavior {
        Echo,
        Fail(&'static str),
    }

    impl Promotable for FakePromotable {
        fn name(&self) -> &str {
            self.name
        }
        fn resolve_track(&self, canonical: &str) -> String {
            match canonical {
                "prerelease" => "beta".to_string(),
                other => other.to_string(),
            }
        }
        fn promote(&self, req: &PromoteRequest) -> anyhow::Result<PromoteOutcome> {
            match self.behavior {
                FakeBehavior::Echo => Ok(PromoteOutcome::promoted(
                    self.name,
                    &req.from,
                    &req.to,
                    req.selector.describe(),
                )),
                FakeBehavior::Fail(msg) => anyhow::bail!("{msg}"),
            }
        }
    }

    fn fake(name: &'static str, behavior: FakeBehavior) -> Box<dyn Promotable> {
        Box::new(FakePromotable { name, behavior })
    }

    #[test]
    fn is_canonical_pretrack_membership() {
        assert!(is_canonical_pretrack("prerelease"));
        assert!(is_canonical_pretrack("candidate"));
        assert!(is_canonical_pretrack("beta"));
        assert!(is_canonical_pretrack("edge"));
        assert!(!is_canonical_pretrack("stable"));
        assert!(!is_canonical_pretrack("canary"));
    }

    #[test]
    fn is_promotion_capable_tracks_the_registry() {
        assert!(is_promotion_capable("snapcraft"));
        assert!(is_promotion_capable("npm"));
        assert!(is_promotion_capable("docker"));
        assert!(is_promotion_capable("github"));
        assert!(!is_promotion_capable("cargo"));
        assert!(!is_promotion_capable("pypi"));
    }

    #[test]
    fn selector_describe_is_stable() {
        assert_eq!(PromoteSelector::Newest.describe(), "newest");
        assert_eq!(
            PromoteSelector::Version("1.2.3".into()).describe(),
            "version 1.2.3"
        );
        assert_eq!(
            PromoteSelector::FromRun {
                run_id: "abc".into(),
                report: PublishReport::default(),
            }
            .describe(),
            "run abc"
        );
    }

    #[test]
    fn dispatch_resolves_tracks_into_native_vocabulary() {
        let ctx = Context::test_fixture();
        let publishers = vec![fake("fake", FakeBehavior::Echo)];
        let report = dispatch_promotions(
            &publishers,
            "prerelease",
            "stable",
            &PromoteSelector::Newest,
            &ctx,
        );
        assert_eq!(report.results.len(), 1);
        let o = &report.results[0];
        // `prerelease` resolved to the publisher-native `beta`; `stable` passed through.
        assert_eq!(o.from, "beta");
        assert_eq!(o.to, "stable");
        assert!(matches!(o.status, PromoteStatus::Promoted));
        assert!(!report.any_failure());
    }

    #[test]
    fn dispatch_records_err_as_failed_without_aborting() {
        let ctx = Context::test_fixture();
        let publishers = vec![
            fake("boom", FakeBehavior::Fail("kaboom")),
            fake("ok", FakeBehavior::Echo),
        ];
        let report = dispatch_promotions(
            &publishers,
            "candidate",
            "stable",
            &PromoteSelector::Version("9.9.9".into()),
            &ctx,
        );
        assert_eq!(report.results.len(), 2, "fan-out continues past a failure");
        assert!(report.any_failure());
        assert_eq!(report.failure_names(), vec!["boom"]);
        let boom = &report.results[0];
        assert!(matches!(boom.status, PromoteStatus::Failed(ref m) if m.contains("kaboom")));
    }

    #[test]
    fn source_label_reflects_selector() {
        assert_eq!(PromoteSelector::Newest.source_label("edge"), "edge");
        assert_eq!(
            PromoteSelector::Version("1.4.0".into()).source_label("edge"),
            "1.4.0"
        );
        assert_eq!(
            PromoteSelector::FromRun {
                run_id: "abc".into(),
                report: PublishReport::default(),
            }
            .source_label("edge"),
            "run abc"
        );
    }

    #[test]
    fn partial_promotion_error_names_applied_and_failed() {
        let msg = partial_promotion_error(
            &["acme/app".to_string(), "acme/lib".to_string()],
            &[("acme/tool".to_string(), "boom".to_string())],
        );
        assert_eq!(
            msg,
            "promoted 2/3 (acme/app, acme/lib); failed on acme/tool: boom"
        );
        let all_failed =
            partial_promotion_error(&[], &[("acme/only".to_string(), "nope".to_string())]);
        assert_eq!(all_failed, "promoted 0/1 (none); failed on acme/only: nope");
    }

    #[test]
    fn summary_line_renders_each_state() {
        let promoted = PromoteOutcome::promoted("snapcraft", "candidate", "stable", "revision 42");
        assert_eq!(
            promoted.summary_line(),
            "snapcraft: revision 42 candidate→stable (promoted)"
        );
        let dry = PromoteOutcome::dry_run("snapcraft", "candidate", "stable", None);
        assert_eq!(dry.summary_line(), "snapcraft: candidate→stable (dry-run)");
        let skipped = PromoteOutcome::skipped(
            "snapcraft",
            "candidate",
            "stable",
            PromoteSkipReason::NothingToPromote,
        );
        assert_eq!(
            skipped.summary_line(),
            "snapcraft: candidate→stable (skipped (nothing to promote))"
        );
        let failed = PromoteOutcome::failed("snapcraft", "candidate", "stable", "boom");
        assert_eq!(
            failed.summary_line(),
            "snapcraft: candidate→stable (failed: boom)"
        );
    }

    #[test]
    fn is_failure_only_true_for_failed() {
        assert!(PromoteOutcome::failed("p", "a", "b", "e").is_failure());
        assert!(!PromoteOutcome::promoted("p", "a", "b", "w").is_failure());
        assert!(!PromoteOutcome::dry_run("p", "a", "b", None).is_failure());
        assert!(
            !PromoteOutcome::skipped("p", "a", "b", PromoteSkipReason::Unsupported).is_failure()
        );
    }
}
