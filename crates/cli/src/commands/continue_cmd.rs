//! `anodizer continue` command.
//!
//! Two modes mirroring GoReleaser Pro's `goreleaser continue`:
//!
//! - **`--merge`** — merge artifacts from split-build workers (each worker
//!   wrote `dist/<target>/context.json` via `anodizer release --split`) and
//!   then run post-build stages (sign / checksum / sbom / release / publish
//!   / announce). Equivalent to GR Pro's `goreleaser continue --merge`.
//!
//! - **no flag** — single-host stage-resume: load artifacts from a populated
//!   `dist/` (typically left over from a `release --prepare` run or a
//!   previous release that failed in publish/announce) and run the
//!   publish-only pipeline (release + publish + blob), then run the announce
//!   stage and after-hooks. Equivalent to GR Pro's `goreleaser continue` and
//!   the long-standing flow for "transient publish failure, retry without
//!   re-building".

use super::helpers;
use crate::pipeline;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use std::path::PathBuf;

pub struct ContinueOpts {
    pub dist: Option<PathBuf>,
    pub dry_run: bool,
    pub skip: Vec<String>,
    pub token: Option<String>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    /// When true, run the merge-mode flow: load every per-target
    /// `context.json` under `dist/`, recombine into a single artifact set,
    /// then run sign / checksum / sbom / release / publish / announce.
    /// When false, run the single-host stage-resume flow: load existing
    /// `dist/` artifacts and re-run publish + announce only.
    pub merge: bool,
}

pub fn run(opts: ContinueOpts) -> Result<()> {
    let log = StageLogger::new(
        "continue",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        token: opts.token,
        merge: opts.merge,
        ..Default::default()
    };

    if opts.merge {
        // Merge-mode does its own per-shard context.json load via run_merge,
        // so init_publish_stage_ctx is the wrong prelude here. Build the
        // context manually with the same setup steps.
        let config_path =
            pipeline::find_config_with_logger(opts.config_override.as_deref(), Some(&log))?;
        let mut config = pipeline::load_config(&config_path)?;
        helpers::infer_project_name(&mut config, &log);
        helpers::auto_detect_github(&mut config, &log);
        let mut ctx = Context::new(config.clone(), ctx_opts);
        helpers::setup_context(&mut ctx, &config, &log)?;
        ctx.populate_metadata_var()?;
        return super::release::run_merge(
            &mut ctx,
            &config,
            &log,
            opts.dry_run,
            opts.dist.as_deref(),
        );
    }

    // Single-host stage-resume: load artifacts from dist/ and run the
    // publish-only pipeline (release + publish + blob), matching the
    // `publish` command's behaviour. This is the GR Pro `goreleaser
    // continue` (no `--merge`) path: a prior release stalled mid-publish
    // (e.g. expired token, transient 5xx) and the user wants to resume
    // without rebuilding.
    let (_config, mut ctx, _dist) = helpers::init_publish_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        opts.dist.as_deref(),
        true,
        &log,
    )?;
    ctx.populate_metadata_var()?;

    let p = pipeline::build_publish_pipeline();
    p.run(&mut ctx, &log)
}
