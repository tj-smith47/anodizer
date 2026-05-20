//! `anodizer announce` command.
//! Runs only the announce stage from a completed dist/.
//! Equivalent to GoReleaser Pro's `goreleaser announce`.

use super::helpers;
use crate::pipeline;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use std::path::PathBuf;

pub struct AnnounceOpts {
    pub dry_run: bool,
    pub dist: Option<PathBuf>,
    pub token: Option<String>,
    pub skip: Vec<String>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

pub fn run(opts: AnnounceOpts) -> Result<()> {
    let log = StageLogger::new(
        "announce",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        token: opts.token,
        ..Default::default()
    };
    let (_config, mut ctx, _dist) = helpers::init_publish_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        opts.dist.as_deref(),
        false,
        &log,
    )?;

    // Run announce-only pipeline
    let p = pipeline::build_announce_pipeline();
    p.run(&mut ctx, &log)
}
