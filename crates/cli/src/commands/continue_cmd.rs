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

    let config_path = pipeline::find_config(opts.config_override.as_deref())?;
    let (mut config, deprecations) = pipeline::load_config_with_deprecations(&config_path)?;
    helpers::infer_project_name(&mut config, &log);
    helpers::auto_detect_github(&mut config, &log);

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        token: opts.token,
        merge: true, // `continue` command always implies --merge
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    for (prop, msg) in &deprecations {
        ctx.deprecate(prop, msg);
    }
    helpers::setup_context(&mut ctx, &config, &log)?;
    ctx.populate_metadata_var();

    super::release::run_merge(&mut ctx, &config, &log, opts.dry_run, opts.dist.as_deref())
}
