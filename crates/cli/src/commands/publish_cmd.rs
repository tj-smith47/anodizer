//! `anodizer publish` command.
//! Runs only the publish stages (release, publish, blob) from a completed dist/.
//! Equivalent to GoReleaser Pro's `goreleaser publish`.

use super::helpers;
use crate::pipeline;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use std::path::PathBuf;

pub struct PublishOpts {
    pub dry_run: bool,
    pub token: Option<String>,
    pub dist: Option<PathBuf>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

pub fn run(opts: PublishOpts) -> Result<()> {
    let log = StageLogger::new(
        "publish",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
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

    // Run publish-only pipeline
    let p = pipeline::build_publish_pipeline();
    p.run(&mut ctx, &log)
}
