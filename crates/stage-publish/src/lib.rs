// Must appear before any module that uses `simple_publisher!` because
// `#[macro_use]` imports macros from this module into the crate-root
// namespace only for siblings that come AFTER it textually.
#[macro_use]
pub(crate) mod publisher_helpers;
pub(crate) mod actions_oidc;
pub(crate) mod publisher_preflight;

pub mod artifactory;
pub mod aur;
pub(crate) mod aur_arch;
pub mod aur_source;
pub mod cargo;
pub mod chocolatey;
pub mod cloudsmith;
pub mod dispatch;
pub mod dockerhub;
pub(crate) mod failure_hooks;
pub mod gemfury;
pub mod homebrew;
pub mod homebrew_core;
pub(crate) mod http_upload;
pub mod krew;
pub mod mcp;
pub mod nix;
pub mod npm;
pub mod post_publish;
pub mod preflight;
pub mod pypi;
pub mod reconcile_report;
pub mod registry;
pub mod rollback;
pub mod run_summary;
pub mod schema_validation;
pub(crate) mod schemastore;
pub mod scoop;
pub(crate) mod scope;
pub(crate) mod snapshot_validation;
pub mod uploads;
pub(crate) mod util;
pub mod winget;

mod poll;
mod report;

/// Test-support module: `FakePublisher`, `FakeOutcome`, etc.
///
/// Gated behind the `test-support` Cargo feature (and `cfg(test)` for
/// the in-crate unit tests). The feature is enabled by this crate's own
/// `[dev-dependencies]` so integration tests under `tests/` can drive
/// the same fakes the in-crate unit tests use.
///
/// NOT a stable public API — shape may change without notice. External
/// consumers outside this workspace MUST NOT rely on it.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod testing;

pub use dispatch::{DispatchOptions, dispatch};
pub use npm::{NpmPromoter, npm_promote_preflight};
pub use registry::{configured_publishers, group_dispatch_order};
pub use schema_validation::{TagResolver, validate_publisher_schemas};

use anodizer_core::config::PublishConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anodizer_core::{PublisherOutcome, SkipReason};
use anyhow::Result;

pub(crate) use poll::run_post_publish_pollers;
#[cfg(test)]
pub(crate) use poll::{PollCandidate, poll_eligibility};
pub(crate) use report::existing_run_report_path;
pub use report::{
    derive_run_id, load_prior_report, report_path_for, run_dir, write_report_to_run_dir,
};

/// Collect crate names that match the selection filter and have a specific
/// publisher configured (as determined by the predicate `has_config`).
///
/// Walks the same crate universe as `cargo.rs::publish_to_cargo` —
/// `ctx.config.crates` plus every `ctx.config.workspaces[].crates` —
/// so a workspace-only crate carrying a non-cargo publisher block
/// (`homebrew:`, `scoop:`, `aur:`, ...) is dispatched alongside the
/// crates from the top-level list. Without this, cargo would publish
/// the workspace crate but every other publisher would silently skip
/// it. See [`anodizer_core::config::Config::crate_universe`] for the
/// dedup rule.
pub(crate) fn crates_with_publisher<F>(
    ctx: &Context,
    selected: &[String],
    has_config: F,
) -> Vec<String>
where
    F: Fn(&PublishConfig) -> bool,
{
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.publish.as_ref().is_some_and(&has_config))
        .map(|c| c.name.clone())
        .collect()
}

/// Fire `on_error` hooks for every failed publisher in `ctx.publish_report`,
/// now that rollback outcomes are final. `rolled_back` is `true` when at
/// least one publisher transitioned to `RolledBack` or `RollbackFailed` —
/// i.e. the failure triggered the rollback path.
fn fire_on_error_hooks(ctx: &Context, log: &StageLogger) {
    let Some(report) = ctx.publish_report() else {
        return;
    };
    let rollback_happened = report.results.iter().any(|r| {
        matches!(
            r.outcome,
            PublisherOutcome::RolledBack | PublisherOutcome::RollbackFailed(_)
        )
    });
    // Clone targets to release the borrow on `report` before calling
    // `fire_on_error`, which itself needs to walk `ctx`.
    let targets: Vec<(anodizer_core::PublisherResult, String)> = report
        .results
        .iter()
        .filter_map(|r| {
            if let PublisherOutcome::Failed(ref err) = r.outcome {
                Some((r.clone(), err.clone()))
            } else {
                None
            }
        })
        .collect();
    let _ = report;
    // Invariant across the fan-out — derive once instead of re-stat'ing the
    // run report per failed publisher.
    let run_report = failure_hooks::run_report_var(ctx);
    for (result, err) in targets {
        failure_hooks::fire_on_error(ctx, &result, &err, rollback_happened, &run_report, log);
    }
}

/// Verify every `--allow-nondeterministic <name>=<reason>` entry
/// matches at least one artifact emitted by the build-side pipeline.
/// Glob entries (`*.ext`) match by suffix; bare names match exactly.
///
/// Called at the top of [`PublishStage::run`] so the run errors out
/// BEFORE any publisher fires. An unmatched name almost always
/// signifies an operator typo — silently letting it through would
/// produce a release with an exemption notice that doesn't apply to
/// anything, undermining the audit trail.
fn validate_runtime_allowlist(ctx: &Context) -> Result<()> {
    let entries = &ctx.options.runtime_nondeterministic_allowlist;
    if entries.is_empty() {
        return Ok(());
    }
    let artifact_names: Vec<&str> = ctx
        .artifacts
        .all()
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    // Also match against the basename of `artifact.path`: the spec
    // encourages operators to type `*.crate` / `*.deb` (file-extension
    // patterns), but `artifact.name` is whatever the build stage
    // recorded and is not always the on-disk filename. Matching both
    // surfaces means a `*.crate` glob hits whichever of
    // `artifact.name` ("anodize-v0.2.1") or
    // `basename(artifact.path)` ("anodize-v0.2.1.crate") satisfies
    // the pattern.
    let artifact_pathnames: Vec<String> = ctx
        .artifacts
        .all()
        .iter()
        .filter_map(|a| a.path.file_name().map(|f| f.to_string_lossy().into_owned()))
        .collect();
    let mut unmatched: Vec<&str> = Vec::new();
    for (name, _reason) in entries {
        let matched = artifact_names
            .iter()
            .any(|n| matches_artifact_pattern(name, n))
            || artifact_pathnames
                .iter()
                .any(|n| matches_artifact_pattern(name, n.as_str()));
        if !matched {
            unmatched.push(name.as_str());
        }
    }
    if !unmatched.is_empty() {
        anyhow::bail!(
            "--allow-nondeterministic name(s) did not match any emitted artifact: {} \
             (check spelling; use `*.ext` glob for suffix match)",
            unmatched.join(", ")
        );
    }
    Ok(())
}

/// Glob match: `*.ext` is suffix-match; anything else is exact-match.
/// Same semantics as `DeterminismState::resolve_reason` (kept local
/// here to avoid exposing the core helper publicly).
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

/// Validates the anodize-only emissions (binstall, nix, version-sync) in
/// every mode — snapshot, dry-run, nightly, and real releases.
///
/// Each emission is rendered in-memory (milliseconds, no side effects) and
/// cross-checked against the assets the run produced, so a broken emission
/// (a 404-class binstall `pkg_url`, a nix system mapped to a missing asset,
/// a crate with no resolvable version) fails BEFORE any publisher ships it —
/// locally in snapshot/dry-run, and ahead of the publish stages in a real
/// release — instead of on a consumer's `cargo binstall` / `nix build`.
///
/// Placed after the packaging + checksum stages so `ctx.artifacts` carries the
/// archive set the cross-checks compare against, and before the publishers so
/// a broken emission aborts the snapshot before any (skipped-anyway) publish
/// work is reported as green.
pub struct EmissionValidateStage;

impl Stage for EmissionValidateStage {
    fn name(&self) -> &str {
        "emission-validate"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        snapshot_validation::validate_emissions(ctx, &log)
    }
}

/// Operator-facing summary line for a publisher recorded as
/// [`SkipReason::Deselected`]. Delegates to the shared
/// [`Context::deselected_reason`] so the dispatch chokepoint, the publish
/// summary, and the out-of-dispatch publish stages all surface identical
/// wording.
fn deselected_skip_line(ctx: &Context, name: &str) -> String {
    ctx.deselected_reason(name)
}

pub struct PublishStage;

impl PublishStage {
    /// Core of `Stage::run`, factored out so tests can substitute an
    /// arbitrary `&[Box<dyn Publisher>]` registry. `Stage::run` calls
    /// this (via [`Self::run_publish_pipeline`]) with
    /// `registry::configured_publishers(ctx)`.
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
    /// This function is `pub` + `#[doc(hidden)]` so the in-crate
    /// `#[cfg(test)] mod tests` block AND the cross-crate integration
    /// test `crates/stage-publish/tests/run_report_persistence.rs` can
    /// substitute a synthetic publisher slice. It is **not** part of
    /// the public API surface: `#[doc(hidden)]` marks that downstream
    /// crates must not couple to this signature; production consumers
    /// should invoke `<PublishStage as Stage>::run` instead. The
    /// integration test depends on this seam by design (writer/reader
    /// contract for `report.json`), so visibility cannot tighten to
    /// `pub(crate)` without breaking that test.
    #[doc(hidden)]
    pub fn run_with_publishers(
        ctx: &mut Context,
        log: &StageLogger,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let opts = dispatch::DispatchOptions {
            fail_fast: ctx.options.fail_fast,
            gate_submitter: ctx.options.gate_submitter.unwrap_or(true),
            persist_snapshots: true,
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
            "publish complete — {} succeeded, {} failed, {} skipped, submitter_gated={}, verify_gate_blocked={}",
            succeeded, failed, skipped, report.submitter_gated, report.verify_gate_blocked,
        ));
        // Per-publisher failure detail — surface error strings so
        // operators see which publisher failed without re-reading the
        // dispatcher's interleaved log output.
        for r in &report.results {
            if let PublisherOutcome::Failed(msg) = &r.outcome {
                log.warn(&format!("publisher {} failed: {}", r.name, msg));
            } else if let PublisherOutcome::Skipped(SkipReason::SubmitterGated) = &r.outcome {
                log.status(&format!("skipped {} via submitter-gate", r.name));
            } else if let PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked) = &r.outcome {
                log.status(&format!(
                    "skipped {} — blocked by the pre-submitter verify-release gate",
                    r.name
                ));
            } else if let PublisherOutcome::Skipped(SkipReason::Deselected) = &r.outcome {
                log.status(&deselected_skip_line(ctx, &r.name));
            }
        }

        ctx.set_publish_report(report);
        Ok(())
    }

    /// Everything `Stage::run` does after the publisher registry is built:
    /// dispatch, `on_error` hooks, report/summary persistence, post-publish
    /// polling, and the in-stage required-failure gate.
    /// Factored out of `Stage::run` so tests can drive the FULL stage
    /// sequence with a synthetic publisher slice and observe the gate's
    /// `Err` alongside the persisted report.
    fn run_publish_pipeline(
        ctx: &mut Context,
        log: &StageLogger,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        Self::run_with_publishers(ctx, log, publishers)?;

        // ---- Persist end-of-pipeline state to dist/run-<id>/report.json ----
        //
        // Writer half of the `anodizer tag rollback` contract (`rollback::run`
        // is the reader). Runs BEFORE `fire_on_error_hooks`, so an operator
        // hook reading the run report (via `$ANODIZER_RUN_REPORT`) observes
        // THIS run's outcomes rather than a previous run's file (hooks only
        // read `&Context`; nothing they do can change reportable state after
        // the write). Snapshot / dry-run modes and empty-result reports are
        // no-ops; IO failure is best-effort (warn + continue, never fail the
        // pipeline).
        write_report_to_run_dir(ctx, log);

        // ---- Fire on_error hooks ----
        fire_on_error_hooks(ctx, log);

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
            run_post_publish_pollers(ctx, &selected, log);
        }

        // ---- In-stage required-failure gate ----
        //
        // Last, AFTER every dispatch / rollback / persistence / polling
        // obligation above has observed final state: the stage itself
        // fails when any required publisher landed in a failure state.
        // The CLI's end-of-pipeline `gate_required_failures` remains as
        // the outer layer of the same defense — this inner gate ensures
        // any embedding of the stage (publish-only, per-crate loops,
        // future pipelines) cannot report a green publish stage over a
        // failed required publisher.
        bail_on_required_failures(ctx)
    }
}

/// The in-stage layer of the shared required-failure exit gate
/// ([`anodizer_core::publish_report::gate_required_failures`]): the skip
/// set, the failure filter, the name list, and the recovery hint all live
/// in core, so this bail and the CLI's end-of-pipeline gate cannot drift.
/// Only the what-completed-before-this-error sentence is stage-specific.
fn bail_on_required_failures(ctx: &Context) -> Result<()> {
    anodizer_core::publish_report::gate_required_failures(
        ctx,
        "All publishers were dispatched and rollback / report / summary \
         bookkeeping completed before this error.",
    )
}

impl Stage for PublishStage {
    fn name(&self) -> &str {
        "publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        // The crate-universe walker is silent (it runs once per predicate /
        // collapse / dispatch call), so its config-shape diagnostics surface
        // here, once per run — before the snapshot skip, because a snapshot
        // preview of a colliding config should surface the mistake too.
        for w in ctx.config.crate_universe_collision_warnings() {
            log.warn(&w);
        }
        if ctx.skip_in_snapshot(&log, "publish") {
            return Ok(());
        }
        // Mark before the guards below: an abort past this point must
        // read "aborted before dispatch" (not "stages skipped") in the
        // summary placeholder row.
        ctx.set_publish_attempted();

        // Preflight: every `--allow-nondeterministic <name>=<reason>`
        // entry must match at least one artifact emitted by the
        // build-side pipeline. Fail hard BEFORE the first publisher
        // fires so an operator typo can't ship as a silent exemption.
        validate_runtime_allowlist(ctx)?;

        // Build the publisher list from the active context and hand off
        // to the group-aware dispatcher via `run_publish_pipeline`.
        // `configured_publishers` is the single source of truth for
        // which publishers run.
        let publishers = registry::configured_publishers(ctx);

        // Refuse to publish a non-release version (snapshot / dirty /
        // 0.0.0-sentinel) to any external publisher. Runs BEFORE the first
        // publisher fires because several are one-way-door indexes; the
        // `--allow-snapshot-publish` flag downgrades the bail to a warning.
        // The same shared guard is wired into the blob and announce stages so a
        // `--skip=publish` run still cannot leak a non-release version.
        let publisher_names: Vec<String> =
            publishers.iter().map(|p| p.name().to_string()).collect();
        anodizer_core::version::guard_release_version(ctx, &log, "publish", &publisher_names)?;

        // Surface the release-optional + dependent-manifest-publisher coupling
        // before any publisher fires (a manifest pointing at a 404 release URL
        // ships silently otherwise).
        registry::warn_release_optional_with_dependent_publisher(ctx, &log);
        Self::run_publish_pipeline(ctx, &log, &publishers)
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
    fn deselected_skip_line_names_the_skip_cause() {
        // A publisher named in `--skip` reports the denylist cause.
        let ctx = Context::new(
            Config::default(),
            ContextOptions {
                skip_stages: vec!["npm".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(
            deselected_skip_line(&ctx, "npm"),
            "skipped npm — excluded via --skip"
        );
    }

    #[test]
    fn deselected_skip_line_names_the_allowlist_cause() {
        // A publisher absent from a non-empty `--publishers` allowlist (and
        // NOT in `--skip`) reports the allowlist cause.
        let ctx = Context::new(
            Config::default(),
            ContextOptions {
                publisher_allowlist: vec!["cargo".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(
            deselected_skip_line(&ctx, "npm"),
            "skipped npm — not in --publishers allowlist"
        );
    }

    #[test]
    fn deselected_skip_line_prefers_skip_when_both_apply() {
        // `--skip` always wins: a publisher in both selectors reports the
        // denylist cause, not the allowlist cause.
        let ctx = Context::new(
            Config::default(),
            ContextOptions {
                skip_stages: vec!["npm".to_string()],
                publisher_allowlist: vec!["cargo".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(
            deselected_skip_line(&ctx, "npm"),
            "skipped npm — excluded via --skip"
        );
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
    // validate_runtime_allowlist — operator-typo guard before publishers fire
    // -----------------------------------------------------------------------

    fn add_artifact(ctx: &mut Context, name: &str) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("dist/{name}")),
            name: name.to_string(),
            target: None,
            crate_name: "test".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    #[test]
    fn allow_nondeterministic_validates_matching_name() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "anodizer-0.1.0.tar.gz".to_string(),
            "embedded build date".to_string(),
        )];
        validate_runtime_allowlist(&ctx).expect("matching name must pass validation");
    }

    #[test]
    fn allow_nondeterministic_validates_matching_glob() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.rpm");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("*.rpm".to_string(), "rpm metadata".to_string())];
        validate_runtime_allowlist(&ctx).expect("matching glob must pass validation");
    }

    #[test]
    fn allow_nondeterministic_unmatched_name_errors_before_publish() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "anodizer-0.1.0.deb".to_string(),
            "typo - meant tar.gz".to_string(),
        )];
        let err =
            validate_runtime_allowlist(&ctx).expect_err("unmatched name must error before publish");
        let msg = err.to_string();
        assert!(
            msg.contains("anodizer-0.1.0.deb"),
            "error must name the unmatched entry: {msg}",
        );
        assert!(
            msg.contains("--allow-nondeterministic"),
            "error must cite the flag for operator orientation: {msg}",
        );
    }

    #[test]
    fn allow_nondeterministic_empty_list_is_noop() {
        let ctx = Context::test_fixture();
        // No allowlist entries, no artifacts — must not error.
        validate_runtime_allowlist(&ctx).expect("empty allowlist must be a no-op");
    }

    /// Helper for tests that need to control `artifact.name` and
    /// `artifact.path` independently — exercising the basename-match
    /// path in `validate_runtime_allowlist`.
    fn add_artifact_with_path(ctx: &mut Context, name: &str, path: &str) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(path),
            name: name.to_string(),
            target: None,
            crate_name: "test".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    #[test]
    fn allow_nondeterministic_matches_file_extension_against_path_basename() {
        // Build stage recorded `artifact.name = "anodize-v0.2.1"` (no
        // extension), while the actual file on disk is
        // `dist/anodize-v0.2.1.crate`. A `*.crate` glob must match via
        // the path-basename surface even though the name alone won't.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "anodize-v0.2.1", "dist/anodize-v0.2.1.crate");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("*.crate".to_string(), "cargo embeds mtime".to_string())];
        validate_runtime_allowlist(&ctx)
            .expect("*.crate glob must match path basename when name lacks extension");
    }

    #[test]
    fn allow_nondeterministic_matches_exact_basename_against_path() {
        // Exact-match form: operator types the full filename. `name`
        // is the bare crate identifier; `path` is the real file.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "core", "dist/core-aarch64.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "core-aarch64.tar.gz".to_string(),
            "tar metadata".to_string(),
        )];
        validate_runtime_allowlist(&ctx).expect("exact basename must match path filename");
    }

    #[test]
    fn allow_nondeterministic_typo_still_errors() {
        // Negative case: a real typo against the same artifact above
        // must still fall through to the unmatched error path — the
        // basename surface widens what *can* match but does not
        // suppress typo detection.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "core", "dist/core-aarch64.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("corre.tar.gz".to_string(), "typo".to_string())];
        let err = validate_runtime_allowlist(&ctx)
            .expect_err("typo must still error even with basename match enabled");
        let msg = err.to_string();
        assert!(
            msg.contains("corre.tar.gz"),
            "error must name the unmatched entry: {msg}",
        );
    }

    // -----------------------------------------------------------------------
    // Dispatch + on_error-hook integration - end-to-end PublishStage::run
    // path through `run_with_publishers` + `fire_on_error_hooks`.
    // -----------------------------------------------------------------------

    /// Helper to drive the same end-to-end shape `Stage::run` exercises
    /// (dispatch -> on_error hooks) but with a synthetic publisher slice.
    /// Skips the post-publish polling fan-out because the fan-out only
    /// reads per-crate config blocks; with no chocolatey/winget blocks
    /// configured, the helper is a no-op.
    fn run_dispatch_and_hooks(
        ctx: &mut Context,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(ctx, &log, publishers)?;
        fire_on_error_hooks(ctx, &log);
        Ok(())
    }

    #[test]
    fn on_error_hook_fires_through_stage_failure_path() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;
        use anodizer_core::config::{CrateConfig, HookEntry, PublishConfig, StructuredHook};
        use anodizer_core::test_helpers::TestContextBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir
            .path()
            .join("fired.txt")
            .display()
            .to_string()
            .replace('\\', "/");
        let publish = PublishConfig {
            on_error: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!("printf '%s\\n' '{{{{ .Publisher }}}}:{{{{ .Error }}}}' >> {out}"),
                ..Default::default()
            })]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![CrateConfig {
                name: "app".into(),
                path: ".".into(),
                publish: Some(publish),
                ..Default::default()
            }])
            .build();
        let publishers: Vec<Box<dyn anodizer_core::Publisher>> = vec![fake(
            "homebrew",
            PublisherGroup::Manager,
            true,
            FakeOutcome::Fail("tap rejected".into()),
        )];
        run_dispatch_and_hooks(&mut ctx, &publishers)
            .expect("dispatch+hooks helper returns Ok; the stage-level gate errors separately");

        let body = std::fs::read_to_string(dir.path().join("fired.txt"))
            .expect("on_error hook must have fired after run_dispatch_and_hooks");
        assert_eq!(body.trim(), "homebrew:tap rejected");
    }

    /// A required publisher failure makes the STAGE itself return Err — the
    /// in-stage defense-in-depth gate — while every bookkeeping obligation
    /// still completes first: all publishers dispatch (no early abort of
    /// siblings, no automatic rollback), and report.json + summary.json
    /// land on disk. The error must name the failed required publisher.
    /// Re-running the pipeline is how a Succeeded sibling converges;
    /// deliberate withdrawal is `anodizer tag rollback`.
    #[test]
    fn publish_stage_errs_on_required_failure_after_persisting_state() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;
        use anodizer_core::test_helpers::TestContextBuilder;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut ctx = TestContextBuilder::new()
            .tag("v0.0.0-gate")
            .dist(tmp.path().to_path_buf())
            .build();
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
        let log = ctx.logger("publish-test");
        let err = PublishStage::run_publish_pipeline(&mut ctx, &log, &publishers)
            .expect_err("a failed required publisher must fail the stage");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("required publisher"),
            "error names the failure class: {msg}"
        );
        assert!(
            msg.contains("manager"),
            "error must name the failed required publisher: {msg}"
        );

        // Bookkeeping completed BEFORE the Err: both publishers dispatched,
        // and the run dir carries report.json + summary.json. No automatic
        // rollback fires — the succeeded Assets row stays Succeeded; a
        // re-run converges it, and deliberate withdrawal is a separate
        // `anodizer tag rollback` invocation.
        let report = ctx.publish_report().expect("publish_report set");
        assert_eq!(report.results.len(), 2, "all publishers dispatched");
        assert!(
            report.results.iter().any(|r| r.name == "assets"
                && matches!(r.outcome, anodizer_core::PublisherOutcome::Succeeded)),
            "no automatic rollback: the succeeded publisher stays Succeeded"
        );
        let run_dir = tmp.path().join("run-v0.0.0-gate");
        assert!(
            run_dir.join("report.json").exists(),
            "report.json must be written despite the required failure"
        );
        assert!(
            run_dir.join("summary.json").exists(),
            "summary.json must be written despite the required failure"
        );
    }

    /// A NON-required publisher failure keeps the stage Ok: the failure is
    /// recorded in the report for the operator (and the summary), but must
    /// not abort the pipeline.
    #[test]
    fn publish_stage_ok_on_non_required_failure() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;
        use anodizer_core::test_helpers::TestContextBuilder;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut ctx = TestContextBuilder::new()
            .tag("v0.0.0-soft")
            .dist(tmp.path().to_path_buf())
            .build();
        let publishers = vec![fake(
            "krew",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Fail("index push rejected".into()),
        )];
        let log = ctx.logger("publish-test");
        PublishStage::run_publish_pipeline(&mut ctx, &log, &publishers)
            .expect("a non-required failure must keep the stage Ok");

        let report = ctx.publish_report().expect("publish_report set");
        let krew = report
            .results
            .iter()
            .find(|r| r.name == "krew")
            .expect("krew entry present");
        assert!(
            matches!(krew.outcome, anodizer_core::PublisherOutcome::Failed(_)),
            "the failure must still be recorded, got {:?}",
            krew.outcome
        );
    }

    #[test]
    fn publish_stage_records_optional_manager_failure_without_touching_assets() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Optional Manager publisher fails; the Assets publisher must be
        // unaffected — dispatch records per-publisher outcomes independently,
        // with no cross-publisher reaction to a non-required failure.
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
        run_dispatch_and_hooks(&mut ctx, &publishers)
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

    /// Pins that the non-release version guard is WIRED into
    /// `PublishStage::run` — not merely that `guard_release_version` works in
    /// isolation. Drives the real `Stage::run` entrypoint with a configured
    /// cargo publisher and a `0.0.0~SNAPSHOT-<sha>` version in a real-release
    /// (non-snapshot, non-dry-run) context, and asserts it bails BEFORE any
    /// publisher fires (`publish_report` stays `None`) with an error naming the
    /// version, a publisher, and the override flag. Deleting the
    /// `guard_release_version` call at the `PublishStage::run` call site makes
    /// this test fail (the run would proceed past the guard into dispatch).
    #[test]
    fn publish_stage_run_bails_on_non_release_version() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        // Real release: NOT snapshot, NOT dry-run, so the guard is live.
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut()
            .set("Version", "0.0.0~SNAPSHOT-d7813f0");

        let err = PublishStage
            .run(&mut ctx)
            .expect_err("a non-release version must bail at PublishStage::run");
        let msg = err.to_string();
        assert!(
            msg.contains("0.0.0~SNAPSHOT-d7813f0"),
            "error must name the offending version: {msg}",
        );
        assert!(
            msg.contains("cargo"),
            "error must name a publisher about to run: {msg}",
        );
        assert!(
            msg.contains("--allow-snapshot-publish"),
            "error must tell the operator how to override: {msg}",
        );
        assert!(
            ctx.publish_report().is_none(),
            "guard must abort BEFORE the dispatcher initializes the publish report",
        );
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
            tag_template: Some("v{{ .Version }}".to_string()),
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

    /// A workspace-only crate that carries a non-cargo publisher block
    /// (homebrew/scoop/aur/...) must be visible to `crates_with_publisher`,
    /// matching the universe `cargo.rs::publish_to_cargo` walks. Under a
    /// `config.crates`-only walk, this crate would silently disappear from
    /// every non-cargo dispatcher even though cargo would still publish it.
    #[test]
    fn test_crates_with_publisher_includes_workspace_only_crates() {
        let mut config = Config::default();
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "ws-only".to_string(),
                path: "crates/ws-only".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
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

    /// Dedup rule: top-level `crates` wins on name collision with a
    /// workspace entry. Both walkers (cargo + non-cargo) must see exactly
    /// one entry per name so `expand_with_transitive_deps` and the
    /// publisher loops never double-publish.
    #[test]
    fn test_crates_with_publisher_dedupes_top_level_over_workspace() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "shared".to_string(),
            path: "top".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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
                tag_template: Some("v{{ .Version }}".to_string()),
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

    /// `PublishStage::run` actually EMITS the universe collision warnings —
    /// the walker itself is silent, so without this call site a name
    /// collision with diverging paths (almost certainly a config mistake)
    /// would be invisible to the operator whose workspace crate is dropped.
    /// Snapshot mode is used because the warnings must surface BEFORE the
    /// snapshot skip.
    #[test]
    fn publish_stage_run_emits_collision_warnings() {
        use anodizer_core::log::{LogCapture, LogLevel};
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "shared".to_string(),
            path: "top".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }];
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                // Same name, DIFFERENT path — the shape the warning flags.
                name: "shared".to_string(),
                path: "ws/shared".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());
        assert!(PublishStage.run(&mut ctx).is_ok());
        let lines = cap.all_messages();
        assert!(
            lines
                .iter()
                .any(|(l, m)| *l == LogLevel::Warn && m.contains("shadowed by")),
            "run() must emit the collision warning: {lines:?}"
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
        // Polling is opt-in per-publisher (PostPublishPollConfig default
        // is `enabled: false` because moderation queues take hours-to-
        // days). The skip-path test must therefore explicitly enable
        // polling on both publisher blocks before asserting that
        // `--no-post-publish-poll` overrides emit `NotPolled` rows for
        // each eligible publisher.
        use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig, WingetConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("mylib-choco".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("TJSmith".to_string()),
                    name: Some("MyLib".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
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

    /// A publisher deselected via the `--publishers` ALLOWLIST (not via a
    /// stage-skip token) is not polled. The allowlist contains only winget,
    /// so chocolatey — though configured with polling enabled — must be
    /// gated out of `run_post_publish_pollers` by
    /// `publisher_deselected("chocolatey")`.
    ///
    /// The observable seam is the same one the sibling skip-path test uses:
    /// with `skip_post_publish_poll: true`, each eligible publisher records a
    /// `NotPolled` row in `ctx.stage_outputs.post_publish_results`. A row is
    /// only ever produced inside the per-publisher
    /// `if !ctx.publisher_deselected(..)` guard, so the absence of any
    /// chocolatey row is direct proof the poller path was never entered for
    /// it. (The CLI flag makes the assertion network-free; the poll guard
    /// under test is independent of that flag.)
    #[test]
    fn allowlist_deselected_publisher_is_not_polled() {
        use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig, WingetConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("mylib-choco".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("TJSmith".to_string()),
                    name: Some("MyLib".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                skip_post_publish_poll: true,
                // Allowlist excludes chocolatey: only winget may be polled.
                // No stage-skip token is involved — this exercises the
                // allowlist arm of `publisher_deselected`, not `should_skip`.
                publisher_allowlist: vec!["winget".to_string()],
                ..Default::default()
            },
        );

        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        run_post_publish_pollers(&mut ctx, &[], &log);

        let results = &ctx.stage_outputs.post_publish_results;
        assert_eq!(
            results.len(),
            1,
            "only the allowlisted publisher (winget) may be polled (got {results:?})"
        );
        assert_eq!(
            results[0]["publisher"], "winget",
            "winget is allowlisted and must still be polled"
        );
        assert!(
            !results.iter().any(|r| r["publisher"] == "chocolatey"),
            "chocolatey is excluded by the allowlist and must never be polled (got {results:?})"
        );
    }

    /// A chocolatey block pushing to a non-community feed must never be
    /// polled: moderation polling scrapes the community gallery's version
    /// page, which carries no signal for a private feed. The observable seam
    /// is the same network-free one the allowlist test uses: with
    /// `skip_post_publish_poll: true` an eligible publisher records a
    /// `NotPolled` row, so the absence of a chocolatey row proves the gate
    /// fired before the poller path.
    #[test]
    fn non_community_choco_feed_is_not_polled() {
        use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("mylib-choco".to_string()),
                    source_repo: Some("https://nuget.internal.example/v2/".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                skip_post_publish_poll: true,
                ..Default::default()
            },
        );

        let (log, cap) = StageLogger::with_capture("test", anodizer_core::log::Verbosity::Quiet);
        run_post_publish_pollers(&mut ctx, &[], &log);

        assert!(
            ctx.stage_outputs.post_publish_results.is_empty(),
            "a non-community feed must not enter the poller path at all (got {:?})",
            ctx.stage_outputs.post_publish_results
        );
        let notes: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == anodizer_core::log::LogLevel::Status)
            .map(|(_, m)| m)
            .collect();
        assert!(
            notes.iter().any(|m| m.contains("not the community gallery")
                && m.contains("mylib-choco")
                && m.contains("nuget.internal.example")),
            "the skip must be summarized at default visibility: {notes:?}"
        );
    }

    /// The chocolatey and winget arms share one `poll_eligibility` ladder, so
    /// for an equivalent config they must yield identical eligibility —
    /// divergence here would gate moderation polling differently on two
    /// irreversible publishers. Asserts parity across the enabled/disabled
    /// filter and both the poll branch and the `--no-post-publish-poll` skip
    /// branch.
    #[test]
    fn choco_and_winget_poll_eligibility_are_identical_for_equivalent_config() {
        use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig, WingetConfig};

        let enabled = || {
            Some(PostPublishPollConfig {
                enabled: true,
                ..Default::default()
            })
        };
        let disabled = || {
            Some(PostPublishPollConfig {
                enabled: false,
                ..Default::default()
            })
        };

        let mut config = Config::default();
        config.crates = vec![
            CrateConfig {
                name: "alpha".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig {
                    chocolatey: Some(ChocolateyConfig {
                        post_publish_poll: enabled(),
                        ..Default::default()
                    }),
                    winget: Some(WingetConfig {
                        post_publish_poll: enabled(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // Polling disabled on both — each arm must filter it out.
            CrateConfig {
                name: "beta".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig {
                    chocolatey: Some(ChocolateyConfig {
                        post_publish_poll: disabled(),
                        ..Default::default()
                    }),
                    winget: Some(WingetConfig {
                        post_publish_poll: disabled(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        let ctx = Context::new(config, ContextOptions::default());

        // `(crate_name, polls?)` is the observable eligibility shape shared by
        // both publishers (the `cfg` payload type differs by construction).
        fn shape<C>(v: &[PollCandidate<C>]) -> Vec<(String, bool)> {
            v.iter()
                .map(|c| (c.crate_name.clone(), c.poll_cfg.is_some()))
                .collect()
        }

        // Poll branch: only the enabled crate, with a resolved poll config.
        let choco = poll_eligibility(&ctx, &[], "chocolatey", false, |p| p.chocolatey.clone());
        let winget = poll_eligibility(&ctx, &[], "winget", false, |p| p.winget.clone());
        assert_eq!(
            shape(&choco),
            vec![("alpha".to_string(), true)],
            "only the enabled crate is eligible, with a poll config"
        );
        assert_eq!(
            shape(&choco),
            shape(&winget),
            "choco and winget poll eligibility must be identical"
        );

        // Skip branch (`--no-post-publish-poll`): same crate set, no poll cfg.
        let choco_skip = poll_eligibility(&ctx, &[], "chocolatey", true, |p| p.chocolatey.clone());
        let winget_skip = poll_eligibility(&ctx, &[], "winget", true, |p| p.winget.clone());
        assert_eq!(
            shape(&choco_skip),
            vec![("alpha".to_string(), false)],
            "skip_via_cli keeps the crate eligible but yields no poll config"
        );
        assert_eq!(
            shape(&choco_skip),
            shape(&winget_skip),
            "choco and winget skip eligibility must be identical"
        );
    }

    #[test]
    fn test_run_dry_run_cargo() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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
    // ── Config-to-behavior wiring tests ──
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_publish_config_is_noop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "nopub".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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
    /// This is a known limitation: homebrew/scoop are skipped for prereleases
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
            tag_template: Some("v{{ .Version }}".to_string()),
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
            tag_template: Some("v{{ .Version }}".to_string()),
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
            tag_template: Some("v{{ .Version }}".to_string()),
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

    // -----------------------------------------------------------------------
    // derive_run_id + write_report_to_run_dir — writer half of the
    // `anodizer tag rollback` contract (`rollback::run` is the reader).
    // Tests below pin: (a) the run_id fallback chain (tag -> short_commit ->
    // "local") with the validator gate, and (b) the writer's no-op/IO
    // behavior including snapshot/dry-run skip and empty-results skip.
    // -----------------------------------------------------------------------

    mod run_report_persistence {
        use super::*;
        use crate::testing::*;
        use anodizer_core::test_helpers::TestContextBuilder;
        use anodizer_core::{
            PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, context::Context,
        };

        fn synthetic_report(name: &str) -> PublishReport {
            let mut r = PublishReport::default();
            r.results.push(PublisherResult {
                name: name.to_string(),
                group: PublisherGroup::Manager,
                required: false,
                outcome: PublisherOutcome::Succeeded,
                evidence: None,
            });
            r
        }

        #[test]
        fn derive_run_id_prefers_tag_when_available() {
            let ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            assert_eq!(derive_run_id(&ctx), "v1.2.3");
        }

        #[test]
        fn derive_run_id_falls_back_to_short_commit_when_tag_empty() {
            let mut ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            // Force the tag empty post-build to exercise the fallback;
            // tag("") would still satisfy validation if non-empty rule
            // were the only check, so blank the field directly.
            ctx.git_info.as_mut().unwrap().tag = String::new();
            assert_eq!(derive_run_id(&ctx), "abc123d");
        }

        #[test]
        fn derive_run_id_falls_back_to_local_when_no_git_info() {
            let mut ctx = TestContextBuilder::new().build();
            ctx.git_info = None;
            assert_eq!(derive_run_id(&ctx), "local");
        }

        #[test]
        fn derive_run_id_falls_back_to_local_when_both_tag_and_short_commit_empty() {
            let mut ctx = TestContextBuilder::new().build();
            let info = ctx.git_info.as_mut().unwrap();
            info.tag = String::new();
            info.short_commit = String::new();
            assert_eq!(derive_run_id(&ctx), "local");
        }

        #[test]
        fn derive_run_id_skips_tag_with_invalid_chars_and_falls_through() {
            // A tag containing '/' (e.g. a malformed monorepo prefix
            // that bypassed earlier validation) must NOT propagate into
            // the run-dir path. Fall through to short_commit.
            let mut ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            ctx.git_info.as_mut().unwrap().tag = "bad/tag".to_string();
            assert_eq!(derive_run_id(&ctx), "abc123d");
        }

        /// A build-metadata tag must NAME the run dir, not fall through to
        /// the commit fallback. The rollback unwind probes `run-<tag>/`
        /// only, so a fallback here silently strands the run's recorded
        /// publisher state while the tag and release are destroyed.
        #[test]
        fn derive_run_id_uses_a_build_metadata_tag_rather_than_the_commit() {
            for tag in ["v1.2.3+build.1", "mycrate-v1.2.3-rc.1+sha.abc"] {
                let mut ctx = TestContextBuilder::new()
                    .tag("v1.2.3")
                    .commit("abc123def4567890")
                    .build();
                ctx.git_info.as_mut().unwrap().tag = tag.to_string();
                assert_eq!(
                    derive_run_id(&ctx),
                    tag,
                    "the writer must key the run dir on {tag}, matching what the unwind probes"
                );
            }
        }

        #[test]
        fn derive_run_id_always_passes_validate_run_id() {
            // Table-driven: every branch of the fallback chain must
            // produce a string that satisfies the validator. A future
            // refactor that loosens an upstream check could regress
            // this — the validator is the single source of truth.
            type CaseFn = fn() -> Context;
            let cases: &[(&str, CaseFn)] = &[
                ("tag branch", || {
                    TestContextBuilder::new()
                        .tag("v0.0.0-test")
                        .commit("abc123def4567890")
                        .build()
                }),
                ("short_commit branch", || {
                    let mut ctx = TestContextBuilder::new()
                        .tag("v0.0.0-test")
                        .commit("abc123def4567890")
                        .build();
                    ctx.git_info.as_mut().unwrap().tag = String::new();
                    ctx
                }),
                ("local fallback (no git_info)", || {
                    let mut ctx = TestContextBuilder::new().build();
                    ctx.git_info = None;
                    ctx
                }),
                ("local fallback (both empty)", || {
                    let mut ctx = TestContextBuilder::new().build();
                    let info = ctx.git_info.as_mut().unwrap();
                    info.tag = String::new();
                    info.short_commit = String::new();
                    ctx
                }),
            ];
            for (label, make_ctx) in cases {
                let ctx = make_ctx();
                let id = derive_run_id(&ctx);
                rollback::validate_run_id(&id).unwrap_or_else(|e| {
                    panic!(
                        "case '{label}' produced invalid run_id '{id}': {e}",
                        label = label,
                        id = id,
                        e = e
                    )
                });
            }
        }

        #[test]
        fn write_report_creates_parent_directory_and_pretty_json() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let path = tmp.path().join("run-v0.0.0-test").join("report.json");
            assert!(path.exists(), "expected report at {}", path.display());

            let body = std::fs::read_to_string(&path).expect("read");
            // Pretty-print includes newlines + 2-space indent. Crude
            // shape-check rather than full whitespace equality so a
            // future serde_json change doesn't break the test.
            assert!(body.contains('\n'), "expected pretty JSON, got: {body}");
            // Round-trip: same shape as PublishReport.
            let parsed: PublishReport = serde_json::from_str(&body).expect("round-trip");
            assert_eq!(parsed.results.len(), 1);
            assert_eq!(parsed.results[0].name, "manager-only");
            assert!(matches!(
                parsed.results[0].outcome,
                PublisherOutcome::Succeeded
            ));
        }

        /// The write-then-fire ordering contract: an `on_error` hook reading
        /// `$ANODIZER_RUN_REPORT` must observe THIS run's report.json — the
        /// stale-file/no-file failure mode was the report being written after
        /// the hooks fired.
        #[test]
        fn on_error_hook_sees_current_run_report_via_env() {
            use anodizer_core::config::{CrateConfig, HookEntry, PublishConfig, StructuredHook};

            let tmp = tempfile::tempdir().expect("tempdir");
            let out = tmp.path().join("hook-out.json");
            let out_sh = out.display().to_string().replace('\\', "/");
            let publish = PublishConfig {
                on_error: Some(vec![HookEntry::Structured(StructuredHook {
                    cmd: format!("cat \"$ANODIZER_RUN_REPORT\" > {out_sh}"),
                    ..Default::default()
                })]),
                ..Default::default()
            };
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .crates(vec![CrateConfig {
                    name: "app".to_string(),
                    path: ".".to_string(),
                    publish: Some(publish),
                    ..Default::default()
                }])
                .build();
            let mut report = PublishReport::default();
            report.results.push(PublisherResult {
                name: "homebrew".to_string(),
                group: PublisherGroup::Manager,
                required: true,
                outcome: PublisherOutcome::Failed("tap push rejected".to_string()),
                evidence: None,
            });
            ctx.set_publish_report(report);

            let log = ctx.logger("publish-test");
            // Same order as run_publish_pipeline: persist, then fire.
            write_report_to_run_dir(&ctx, &log);
            fire_on_error_hooks(&ctx, &log);

            let body = std::fs::read_to_string(&out)
                .expect("hook must have read the run report via $ANODIZER_RUN_REPORT");
            let parsed: PublishReport = serde_json::from_str(&body).expect("current-run JSON");
            assert_eq!(parsed.results[0].name, "homebrew");
            assert!(matches!(
                parsed.results[0].outcome,
                PublisherOutcome::Failed(_)
            ));
        }

        #[test]
        fn write_report_is_noop_on_empty_results() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            // Default report = empty results.
            ctx.set_publish_report(PublishReport::default());

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "no work done -> no dir written; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_in_snapshot_mode() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .snapshot(true)
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "snapshot mode must not pollute dist/run-*/; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_in_dry_run_mode() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .dry_run(true)
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "dry-run mode must not pollute dist/run-*/; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_when_no_publish_report_set() {
            // Edge case: PublishStage::run only calls write after
            // dispatch sets publish_report, but write_report_to_run_dir
            // is defensive against being invoked with no report. Verify
            // the no-op path so a future refactor that moves the call
            // can't crash on None.
            let tmp = tempfile::tempdir().expect("tempdir");
            let ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            // No set_publish_report() call.
            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);
            assert!(!tmp.path().join("run-v0.0.0-test").exists());
        }

        #[test]
        fn publish_stage_run_writes_report_at_end_of_pipeline() {
            // End-to-end via run_with_publishers + write_report_to_run_dir.
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            let publishers = vec![fake(
                "manager-only",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            )];
            let log = ctx.logger("publish-test");
            PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
                .expect("run_with_publishers Ok");
            write_report_to_run_dir(&ctx, &log);

            let path = tmp.path().join("run-v0.0.0-test").join("report.json");
            assert!(path.exists(), "expected report at {}", path.display());
            let parsed: PublishReport =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(parsed.results.len(), 1);
            assert_eq!(parsed.results[0].name, "manager-only");
        }
    }

    #[test]
    fn test_run_dry_run_nix() {
        use anodizer_core::config::{NixConfig, RepositoryConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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
