//! `anodize release --announce-only`: re-fire the announce stage
//! against a `<dist>/run-<id>/report.json` written by a prior
//! end-to-end release run.
//!
//! Use case: a transient announcer failure (Slack 502, Discord 5xx)
//! after a successful publish. The operator wants to retry
//! notifications without re-creating the GitHub release or
//! re-uploading archives — every other stage in the pipeline is
//! skipped.
//!
//! Idempotence properties:
//! - The announce stage itself short-circuits on nightly (see
//!   `crates/stage-announce/src/run.rs::announce_body`), so
//!   `release --announce-only` on a nightly tag is a graceful no-op.
//! - Re-running `--announce-only` against the same `report.json`
//!   re-fires every configured announcer. Announcers that the
//!   prior run succeeded against will post duplicates; the operator
//!   is expected to use this flag only when they explicitly want
//!   that.
//! - After-hooks run on success too — symmetric with the post-pipeline
//!   path of the full release flow.

use anyhow::Result;

use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::pipeline;

/// `--announce-only` entry point. Wired from `commands/release/mod.rs::run`
/// after `setup_context` / git context have resolved the per-run id.
pub(super) fn run(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    dry_run: bool,
) -> Result<()> {
    log.status("running in announce-only mode (load prior report + re-fire announcers)...");

    // Derive run_id from the same git_info the writer uses so the
    // reader/writer agree on the `<dist>/run-<id>/` path without
    // operator input. `--from-run` is intentionally not plumbed here:
    // announce-only's use case (re-fire after transient failure) is
    // always "the run I just finished," not "an arbitrary historical
    // run."
    let run_id = anodizer_stage_publish::derive_run_id(ctx);
    let report = anodizer_stage_publish::load_prior_report(ctx, &run_id)?;
    log.status(&format!(
        "announce-only: loaded prior report (run_id={}, {} publisher result(s))",
        run_id,
        report.results.len()
    ));
    ctx.set_publish_report(report);

    let p = pipeline::build_announce_pipeline();
    p.run(ctx, log)?;

    super::run_post_pipeline_after_hooks_only(ctx, config, dry_run, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    fn ctx_with_dist(dist: std::path::PathBuf) -> Context {
        let config = Config {
            project_name: "test".to_string(),
            dist,
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    #[test]
    fn missing_report_returns_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_dist(tmp.path().to_path_buf());
        let config = ctx.config.clone();
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let err = run(&mut ctx, &config, &log, true).unwrap_err().to_string();
        assert!(
            err.contains("no prior report found"),
            "error must name the missing report: {err}"
        );
    }
}
