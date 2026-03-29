//! `anodize continue --merge` command.
//! Equivalent to GoReleaser Pro's `goreleaser continue --merge`.

use super::helpers;
use crate::pipeline;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::log::{StageLogger, Verbosity};
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
}

pub fn run(opts: ContinueOpts) -> Result<()> {
    let log = StageLogger::new(
        "continue",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let mut config =
        pipeline::load_config(&pipeline::find_config(opts.config_override.as_deref())?)?;
    helpers::auto_detect_github(&mut config, &log);

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        token: opts.token,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    helpers::setup_env(&mut ctx, &config, &log)?;
    helpers::resolve_git_context(&mut ctx, &config, &log);

    super::release::run_merge(&mut ctx, &config, &log, opts.dry_run, opts.dist.as_deref())
}
