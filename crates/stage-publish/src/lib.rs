// Must appear before any module that uses `simple_publisher!` because
// `#[macro_use]` imports macros from this module into the crate-root
// namespace only for siblings that come AFTER it textually.
#[macro_use]
pub(crate) mod publisher_helpers;

pub mod artifactory;
pub mod aur;
pub mod aur_source;
pub mod cargo;
pub mod chocolatey;
pub mod cloudsmith;
pub mod dispatch;
pub mod dockerhub;
pub mod homebrew;
pub(crate) mod http_upload;
pub mod krew;
pub mod mcp;
pub mod nix;
pub mod post_publish;
pub mod preflight;
pub mod registry;
pub mod rollback;
pub mod rollback_only;
pub mod scoop;
pub mod upload;
pub(crate) mod util;
pub mod winget;

#[cfg(test)]
pub(crate) mod testing;

pub use dispatch::{DispatchOptions, dispatch};
pub use registry::{configured_publishers, group_dispatch_order};

use anodizer_core::config::PublishConfig;
use anodizer_core::context::{Context, RollbackMode};
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anodizer_core::{Publisher, PublisherGroup, PublisherOutcome, SkipReason};
use anyhow::Result;

/// Collect crate names that match the selection filter and have a specific
/// publisher configured (as determined by the predicate `has_config`).
///
/// Walks the same crate universe as `cargo.rs::publish_to_cargo` —
/// `ctx.config.crates` plus every `ctx.config.workspaces[].crates` —
/// so a workspace-only crate carrying a non-cargo publisher block
/// (`homebrew:`, `scoop:`, `aur:`, ...) is dispatched alongside the
/// crates from the top-level list. Without this, cargo would publish
/// the workspace crate but every other publisher would silently skip
/// it. See `util::all_crates` for the dedup rule.
fn crates_with_publisher<F>(ctx: &Context, selected: &[String], has_config: F) -> Vec<String>
where
    F: Fn(&PublishConfig) -> bool,
{
    util::all_crates(ctx)
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.publish.as_ref().is_some_and(&has_config))
        .map(|c| c.name)
        .collect()
}

/// Build the post-publish polling job list from the active context and run
/// every job in parallel. Writes typed `PostPublishResult` entries (as JSON
/// values) into `ctx.stage_outputs.post_publish_results` for the deferred
/// release-summary renderer to consume.
///
/// Eligibility rules:
///
/// - The publish stage must NOT be in dry-run / snapshot mode (gated at
///   the call site — nothing was actually pushed in those modes).
/// - Chocolatey jobs require `--skip=choco` to be absent AND a per-crate
///   `chocolatey:` block with `post_publish_poll.enabled != false`.
/// - WinGet jobs require `--skip=winget` to be absent AND a per-crate
///   `winget:` block with `post_publish_poll.enabled != false`.
/// - `--no-post-publish-poll` short-circuits to a `NotPolled` result per
///   eligible publisher (so the release summary can render "skipped"
///   distinctly from "no publishers configured").
///
/// All polling is non-fatal; any worker error becomes a
/// `PostPublishStatus::Error` in the results vec rather than failing the
/// publish stage.
fn run_post_publish_pollers(ctx: &mut Context, selected: &[String], log: &StageLogger) {
    let version = ctx.version();
    let mut jobs: Vec<post_publish::PollJob> = Vec::new();
    // Mirrors `jobs` for the skip-path: when the CLI flag is set we
    // never construct a `PollJob` (no cfg / no URL / no token needed),
    // but we DO want to emit a `NotPolled` result per configured
    // publisher so summaries can render "skipped via flag" vs. "no
    // publishers configured" distinctly. `(publisher, package, version)`
    // triples are collected in dispatch order to match the result vec
    // ordering invariant.
    let mut skipped: Vec<(&'static str, String, String)> = Vec::new();
    let skip_via_cli = ctx.options.skip_post_publish_poll;

    // Chocolatey eligibility — collect a job per per-crate `chocolatey:`
    // block when the `choco` skip isn't engaged.
    if !ctx.should_skip("choco") {
        for crate_name in
            &crates_with_publisher(ctx, selected, |p: &PublishConfig| p.chocolatey.is_some())
        {
            let cfg_opt = util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.chocolatey);
            let Some(choco) = cfg_opt else {
                continue;
            };
            // Per-publisher `enabled: false` opts a publisher out
            // *entirely* (not the same surface as `--no-post-publish-poll`,
            // which is a global skip). Detect that here so the skip-path
            // doesn't emit `NotPolled` for a publisher the operator
            // explicitly turned off in config (which the renderer would
            // otherwise misreport as "skipped via flag").
            let per_pub_cfg = choco.post_publish_poll.unwrap_or_default();
            if !per_pub_cfg.enabled {
                continue;
            }
            let pkg_name = choco.name.unwrap_or_else(|| crate_name.clone());
            if skip_via_cli {
                skipped.push(("chocolatey", pkg_name, version.clone()));
                continue;
            }
            // `resolve_poll_config` collapses both gates (CLI + per-pub)
            // into one `Option`. We've already filtered the per-pub
            // `enabled` case, so a `None` here can only mean the CLI
            // flag — caught by the `skip_via_cli` branch above.
            let Some(poll_cfg) = post_publish::resolve_poll_config(ctx, choco.post_publish_poll)
            else {
                continue;
            };
            jobs.push(post_publish::PollJob::Chocolatey {
                package: pkg_name,
                version: version.clone(),
                page_base_url: "https://community.chocolatey.org".to_string(),
                cfg: poll_cfg,
            });
        }
    }

    // WinGet eligibility — same pattern. The PR is rediscovered via the
    // GitHub search API (mirroring `preflight::Winget`), so we don't need
    // to thread a PR URL through from the publish step.
    if !ctx.should_skip("winget") {
        for crate_name in
            &crates_with_publisher(ctx, selected, |p: &PublishConfig| p.winget.is_some())
        {
            let cfg_opt = util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.winget);
            let Some(winget) = cfg_opt else {
                continue;
            };
            // Per-publisher disable check — same rationale as the
            // chocolatey arm above.
            let per_pub_cfg = winget.post_publish_poll.unwrap_or_default();
            if !per_pub_cfg.enabled {
                continue;
            }
            // PackageIdentifier resolution: prefer explicit
            // `package_identifier`, fall back to `<publisher>.<name>`
            // (the upstream convention enforced by winget validation),
            // then to the crate name as a last resort.
            let pkg_id = winget.package_identifier.clone().unwrap_or_else(|| {
                let publisher = winget.publisher.as_deref().unwrap_or("");
                let name = winget
                    .name
                    .as_deref()
                    .or(winget.package_name.as_deref())
                    .unwrap_or(crate_name);
                if publisher.is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{}", publisher, name)
                }
            });
            if skip_via_cli {
                skipped.push(("winget", pkg_id, version.clone()));
                continue;
            }
            let Some(poll_cfg) = post_publish::resolve_poll_config(ctx, winget.post_publish_poll)
            else {
                continue;
            };
            let token = winget
                .repository
                .as_ref()
                .and_then(|r| r.token.clone())
                .or_else(|| std::env::var("ANODIZER_GITHUB_TOKEN").ok())
                .or_else(|| std::env::var("GITHUB_TOKEN").ok());
            jobs.push(post_publish::PollJob::Winget {
                package_identifier: pkg_id,
                version: version.clone(),
                api_base_url: "https://api.github.com".to_string(),
                token,
                cfg: poll_cfg,
            });
        }
    }

    // Skip-path: emit one `NotPolled` per eligible publisher so the
    // release summary distinguishes "skipped via --no-post-publish-poll"
    // from "no eligible publishers". Short-circuits without running any
    // pollers.
    if skip_via_cli {
        if skipped.is_empty() {
            log.verbose(
                "post-publish polling: skipped via --no-post-publish-poll (no eligible publishers)",
            );
            return;
        }
        log.verbose(&format!(
            "post-publish polling: skipped via --no-post-publish-poll ({} publisher(s) recorded as NotPolled)",
            skipped.len()
        ));
        let not_polled: Vec<post_publish::PostPublishResult> = skipped
            .into_iter()
            .map(
                |(publisher, package, version)| post_publish::PostPublishResult {
                    publisher: publisher.to_string(),
                    package,
                    version,
                    status: post_publish::PostPublishStatus::NotPolled,
                },
            )
            .collect();
        ctx.stage_outputs.post_publish_results = not_polled
            .iter()
            .map(|r| {
                serde_json::to_value(r).expect(
                    "PostPublishResult is always serializable — schema is derived from a string + enum struct",
                )
            })
            .collect();
        return;
    }

    if jobs.is_empty() {
        log.verbose("post-publish polling: no eligible publishers");
        return;
    }
    log.status(&format!(
        "post-publish polling: starting {} parallel poller(s)",
        jobs.len()
    ));
    let results = post_publish::run_post_publish_polls(jobs, log);
    for r in &results {
        match &r.status {
            post_publish::PostPublishStatus::Approved { detail } => log.status(&format!(
                "post-publish: {} {} {} approved: {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Rejected { detail } => log.warn(&format!(
                "post-publish: {} {} {} rejected: {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Timeout { last_state, .. } => log.warn(&format!(
                "post-publish: {} {} {} polling timed out (last state: {})",
                r.publisher, r.package, r.version, last_state
            )),
            post_publish::PostPublishStatus::Error { reason } => log.warn(&format!(
                "post-publish: {} {} {} polling error: {}",
                r.publisher, r.package, r.version, reason
            )),
            post_publish::PostPublishStatus::Pending { .. }
            | post_publish::PostPublishStatus::NotPolled => {
                // Pending shouldn't reach this path (poller loops until
                // terminal). NotPolled is built by callers that explicitly
                // opt out — silent is fine.
            }
        }
    }
    ctx.stage_outputs.post_publish_results = results
        .into_iter()
        .map(|r| {
            serde_json::to_value(&r).expect(
                "PostPublishResult is always serializable — schema is derived from a string + enum struct",
            )
        })
        .collect();
}

/// Run best-effort rollback when the trigger conditions are met:
///
/// 1. At least one required Assets or Manager publisher failed, AND
/// 2. `ctx.options.rollback_mode != Some(RollbackMode::None)`.
///
/// When `ctx.options.rollback_mode` is `None`, this defaults to
/// `RollbackMode::BestEffort` so the rollback path engages by default.
/// The `--rollback=none` CLI flag plumbs `Some(RollbackMode::None)` here
/// to suppress rollback entirely.
///
/// The function takes `ctx.publish_report` out, mutates it via
/// `rollback::run`, and writes it back. Doing the dance here keeps the
/// `&mut Context` borrow scope tight: `rollback::run` needs mutable
/// access to `ctx` (each publisher's `rollback()` is `&mut Context`),
/// which would otherwise conflict with an active `&mut PublishReport`
/// borrow through `ctx.publish_report`.
fn run_rollback_if_needed(ctx: &mut Context, publishers: &[Box<dyn Publisher>], log: &StageLogger) {
    let mode = ctx
        .options
        .rollback_mode
        .unwrap_or(RollbackMode::BestEffort);
    if mode == RollbackMode::None {
        return;
    }

    let needs_rollback = ctx.publish_report.as_ref().is_some_and(|r| {
        r.any_failed(PublisherGroup::Assets, true) || r.any_failed(PublisherGroup::Manager, true)
    });
    if !needs_rollback {
        return;
    }

    log.status("rollback: required failure(s) detected; invoking best-effort rollback");

    // Take the report out so `rollback::run` can mutate it while
    // calling `publisher.rollback(ctx, ...)` (which itself needs
    // `&mut Context`). We unconditionally write it back below.
    let Some(mut report) = ctx.publish_report.take() else {
        // Defensive: `needs_rollback` was true above, so the Option
        // must have been `Some`. If a future refactor changes that
        // invariant, log and bail rather than panic.
        log.warn("rollback: publish_report missing; nothing to dispatch");
        return;
    };
    rollback::run(publishers, &mut report, ctx, mode);
    ctx.set_publish_report(report);
}

pub struct PublishStage;

impl PublishStage {
    /// Core of `Stage::run`, factored out so tests can substitute an
    /// arbitrary `&[Box<dyn Publisher>]` registry. `Stage::run` calls
    /// this with `registry::configured_publishers(ctx)`.
    ///
    /// The body invokes the group-aware dispatcher (Assets -> Manager
    /// -> Submitter, with Submitter gating), writes the resulting
    /// `PublishReport` to `ctx.publish_report`, and returns `Ok(())`
    /// even on per-publisher failure — those failures are recorded in
    /// the report. `Err` is reserved for catastrophic non-publisher
    /// errors (impossible IO, malformed config); for now `dispatch`
    /// itself never returns `Err`.
    ///
    /// # Stability
    ///
    /// This function is `pub` only so the in-crate `#[cfg(test)] mod
    /// tests` block (and any future integration test in
    /// `crates/stage-publish/tests/`) can substitute a synthetic
    /// publisher slice. It is **not** part of the public API surface —
    /// `#[doc(hidden)]` is the marker that downstream crates must not
    /// couple to this signature; consumers should invoke
    /// `<PublishStage as Stage>::run` instead.
    #[doc(hidden)]
    pub fn run_with_publishers(
        ctx: &mut Context,
        log: &StageLogger,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let opts = dispatch::DispatchOptions {
            fail_fast: ctx.options.fail_fast,
            gate_submitter: ctx.options.gate_submitter.unwrap_or(true),
        };
        let report = dispatch::dispatch(publishers, ctx, &opts)?;

        // Summary line — operators see succeeded/failed/skipped counts
        // and whether the submitter gate fired without grepping the
        // per-publisher log noise.
        let succeeded = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Succeeded))
            .count();
        let failed = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Failed(_)))
            .count();
        let skipped = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Skipped(_)))
            .count();
        log.status(&format!(
            "publish: {} succeeded, {} failed, {} skipped, submitter_gated={}",
            succeeded, failed, skipped, report.submitter_gated,
        ));
        // Per-publisher failure detail — surface error strings so
        // operators see which publisher failed without re-reading the
        // dispatcher's interleaved log output.
        for r in &report.results {
            if let PublisherOutcome::Failed(msg) = &r.outcome {
                log.warn(&format!("{}: {}", r.name, msg));
            } else if let PublisherOutcome::Skipped(SkipReason::SubmitterGated) = &r.outcome {
                log.status(&format!("{}: skipped via submitter-gate", r.name));
            }
        }

        ctx.set_publish_report(report);
        Ok(())
    }
}

impl Stage for PublishStage {
    fn name(&self) -> &str {
        "publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        if ctx.skip_in_snapshot(&log, "publish") {
            return Ok(());
        }

        // Build the publisher list from the active context and hand off
        // to the group-aware dispatcher via `run_with_publishers`.
        // `configured_publishers` is the single source of truth for
        // which publishers run.
        let publishers = registry::configured_publishers(ctx);
        Self::run_with_publishers(ctx, &log, &publishers)?;

        // ---- Best-effort rollback dispatch ----
        //
        // Runs only when a required Assets/Manager publisher failed AND
        // the operator did not opt out via `--rollback=none`. Reversible
        // publishers (Assets/Manager) that recorded `Succeeded` get
        // their `Publisher::rollback` invoked; per-step outcomes flip
        // to `RolledBack` / `RollbackFailed` / `RollbackSkippedNoScope`
        // in the report so the run-summary task and downstream stages
        // can render the final state. Submitter publishers are never
        // rolled back (they are protected by the dispatch-time
        // submitter gate).
        run_rollback_if_needed(ctx, &publishers, &log);

        // ---- Post-publish polling fan-out (Chocolatey moderation + WinGet PR) ----
        //
        // Runs AFTER every publisher has completed so polling isn't gated
        // on a failed unrelated publisher (e.g. krew). The fan-out is
        // gated by `--no-post-publish-poll` and by each publisher's
        // `post_publish_poll.enabled` block. Skipping `choco` /
        // `winget` skips their poll automatically (no submission =
        // nothing to poll for).
        if !ctx.is_dry_run() && !ctx.is_snapshot() {
            let selected = ctx.options.selected_crates.clone();
            run_post_publish_pollers(ctx, &selected, &log);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        AurConfig, CargoPublishConfig, Config, CrateConfig, HomebrewConfig, PublishConfig,
        WorkspaceConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_name() {
        assert_eq!(PublishStage.name(), "publish");
    }

    #[test]
    fn test_run_no_crates_configured() {
        let config = Config::default();
        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // PublishStage::run swap — trait-based dispatch sets ctx.publish_report,
    // returns Ok(()) on per-publisher failure, applies the Submitter gate.
    // -----------------------------------------------------------------------

    #[test]
    fn publish_stage_returns_ok_and_sets_context_publish_report() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "manager-only",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Succeed,
        )];
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("run_with_publishers returns Ok on per-publisher success");

        let report = ctx.publish_report().expect("publish_report set on Context");
        assert_eq!(report.results.len(), 1);
        assert!(matches!(
            report.results[0].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
        assert!(!report.submitter_gated);
    }

    // -----------------------------------------------------------------------
    // Rollback dispatch integration - end-to-end PublishStage::run path
    // through `run_with_publishers` + `run_rollback_if_needed`.
    // -----------------------------------------------------------------------

    /// Helper to drive the same end-to-end shape `Stage::run` exercises
    /// (dispatch -> rollback) but with a synthetic publisher slice.
    /// Skips the post-publish polling fan-out because the fan-out only
    /// reads per-crate config blocks; with no chocolatey/winget blocks
    /// configured, the helper is a no-op.
    fn run_dispatch_and_rollback(
        ctx: &mut Context,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(ctx, &log, publishers)?;
        run_rollback_if_needed(ctx, publishers, &log);
        Ok(())
    }

    #[test]
    fn publish_stage_invokes_rollback_after_required_failure() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Required Manager publisher fails; Assets publisher succeeds.
        // Rollback dispatch should flip the Assets entry to RolledBack.
        let publishers = vec![
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok even when required publisher fails");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::RolledBack),
            "expected Assets publisher to be rolled back, got {:?}",
            assets.outcome
        );
        // Manager remains Failed (rollback doesn't touch failed entries).
        let manager = report
            .results
            .iter()
            .find(|r| r.name == "manager")
            .expect("manager entry present");
        assert!(matches!(
            manager.outcome,
            anodizer_core::PublisherOutcome::Failed(_)
        ));
    }

    #[test]
    fn publish_stage_skips_rollback_when_mode_is_none() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            ContextOptions {
                rollback_mode: Some(RollbackMode::None),
                ..Default::default()
            },
        );
        // Same fixture as the prior test, but `--rollback=none`
        // (Some(None)) suppresses the rollback dispatch.
        let publishers = vec![
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok in rollback=none mode");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::Succeeded),
            "expected Assets publisher to remain Succeeded under rollback=none, got {:?}",
            assets.outcome
        );
    }

    #[test]
    fn publish_stage_skips_rollback_when_no_required_failure() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Optional Manager publisher fails - rollback should NOT fire
        // because no REQUIRED publisher failed.
        let publishers = vec![
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
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok on optional failure");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::Succeeded),
            "expected Assets publisher to remain Succeeded when no required failure, got {:?}",
            assets.outcome
        );
    }

    #[test]
    fn publish_stage_records_publisher_failures_without_returning_err() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Three publishers in the Manager group; the middle one fails.
        // Dispatch must record every outcome and still return Ok so the
        // pipeline continues past PublishStage.
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
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("per-publisher failure must not bail the stage");

        let report = ctx.publish_report().expect("publish_report set on Context");
        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["m1", "m2", "m3"]);
        assert!(matches!(
            report.results[0].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
        match &report.results[1].outcome {
            anodizer_core::PublisherOutcome::Failed(msg) => assert!(msg.contains("boom")),
            other => panic!("expected Failed for m2, got {:?}", other),
        }
        assert!(matches!(
            report.results[2].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn submitter_gate_records_skipped_when_required_manager_fails() {
        use crate::testing::*;
        use anodizer_core::{PublisherGroup, PublisherOutcome, SkipReason};

        let mut ctx = Context::test_fixture();
        // Required Manager publisher fails -> Submitter must be gated to
        // Skipped(SubmitterGated) (irreversible publish protected).
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
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("Submitter gating must record skipped, not Err");

        let report = ctx.publish_report().expect("publish_report set on Context");
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
    }

    #[test]
    fn publish_stage_skips_under_snapshot() {
        // Snapshot mode short-circuits `Stage::run` before dispatch fires,
        // leaving `ctx.publish_report` as `None`. This pins the gate in
        // `ctx.skip_in_snapshot` so a future refactor can't silently
        // start running publishers under `--snapshot`.
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        assert!(PublishStage.run(&mut ctx).is_ok());
        assert!(
            ctx.publish_report().is_none(),
            "snapshot mode must short-circuit before dispatch fires"
        );
    }

    /// WAVE 3: a workspace-only crate that carries a non-cargo publisher block
    /// (homebrew/scoop/aur/...) must be visible to `crates_with_publisher`,
    /// matching the universe `cargo.rs::publish_to_cargo` walks. Before the
    /// shared `util::all_crates` lift, this crate would silently disappear
    /// from every non-cargo dispatcher even though cargo would still publish it.
    #[test]
    fn test_crates_with_publisher_includes_workspace_only_crates() {
        let mut config = Config::default();
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "ws-only".to_string(),
                path: "crates/ws-only".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(names, vec!["ws-only".to_string()]);
    }

    /// WAVE 3 dedup rule: top-level `crates` wins on name collision with a
    /// workspace entry. Both walkers (cargo + non-cargo) must see exactly
    /// one entry per name so `expand_with_transitive_deps` and the
    /// publisher loops never double-publish.
    #[test]
    fn test_crates_with_publisher_dedupes_top_level_over_workspace() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "shared".to_string(),
            path: "top".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                // Same name as the top-level — top-level must win.
                name: "shared".to_string(),
                path: "ws/shared".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: None,
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(
            names,
            vec!["shared".to_string()],
            "top-level entry must win on name collision and not be doubled"
        );
    }

    /// `--no-post-publish-poll` must emit one `PostPublishResult { status:
    /// NotPolled }` per eligible per-crate publisher block instead of silently
    /// short-circuiting. The release-summary renderer relies on the explicit
    /// `NotPolled` rows to distinguish "skipped via flag" from "no eligible
    /// publishers" — see `post_publish::status::PostPublishStatus::NotPolled`
    /// docs.
    #[test]
    fn skip_path_emits_not_polled_for_each_configured_publisher() {
        use anodizer_core::config::{ChocolateyConfig, WingetConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("mylib-choco".to_string()),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("TJSmith".to_string()),
                    name: Some("MyLib".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                // NOT dry_run — we want the skip-path inside
                // `run_post_publish_pollers` to engage and emit
                // `NotPolled`. dry-run gates the entire pipeline before
                // ever reaching the post-publish call site.
                skip_post_publish_poll: true,
                ..Default::default()
            },
        );

        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        run_post_publish_pollers(&mut ctx, &[], &log);

        let results = &ctx.stage_outputs.post_publish_results;
        assert_eq!(
            results.len(),
            2,
            "skip path must emit one NotPolled per configured publisher (got {results:?})"
        );

        // Dispatch order in `run_post_publish_pollers`: chocolatey arm
        // runs before winget arm.
        assert_eq!(results[0]["publisher"], "chocolatey");
        assert_eq!(results[0]["package"], "mylib-choco");
        assert_eq!(results[0]["status"]["kind"], "not_polled");

        assert_eq!(results[1]["publisher"], "winget");
        assert_eq!(results[1]["package"], "TJSmith.MyLib");
        assert_eq!(results[1]["status"]["kind"], "not_polled");
    }

    #[test]
    fn test_run_dry_run_cargo() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // dry-run: should log but not actually shell out
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_publish_config_is_noop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "nopub".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: None, // No publish config
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // Should succeed (no-op)
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    /// Document current behavior: the publish stage does NOT skip homebrew/scoop
    /// publishing for prerelease versions. It proceeds regardless of whether
    /// the version contains a prerelease suffix like -rc.1 or -beta.
    ///
    /// This is a known limitation: GoReleaser skips homebrew/scoop for prereleases
    /// by default. If this behavior is added in the future, this test should be
    /// updated to verify that skipping occurs.
    // -----------------------------------------------------------------------
    // Chocolatey integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // WinGet integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // AUR integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_aur() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Krew integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Top-level AUR sources integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_top_level_aur_sources() {
        use anodizer_core::config::AurSourceConfig;

        let mut config = Config::default();
        config.aur_sources = Some(vec![AurSourceConfig {
            name: Some("myapp".to_string()),
            description: Some("My application".to_string()),
            license: Some("MIT".to_string()),
            git_url: Some("ssh://aur@aur.archlinux.org/myapp.git".to_string()),
            makedepends: Some(vec!["rust".to_string(), "cargo".to_string()]),
            ..Default::default()
        }]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_empty_is_noop() {
        let mut config = Config::default();
        config.aur_sources = Some(vec![]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_none_is_noop() {
        let mut config = Config::default();
        config.aur_sources = None;

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Nix integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // record_publisher_result tests removed when PublishStage swapped to
    // trait-based dispatch (see `crates/stage-publish/src/dispatch.rs`).
    // The collect-or-bail policy now lives in `DispatchOptions::fail_fast`
    // and is covered by tests in `crates/stage-publish/src/dispatch.rs`.
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_nix() {
        use anodizer_core::config::{NixConfig, RepositoryConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("myorg".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }
}
