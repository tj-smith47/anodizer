//! `anodize publish` command.
//! Runs only the publish stages (release, publish, blob) from a completed dist/.
//! Equivalent to GoReleaser Pro's `goreleaser publish`.

use super::helpers;
use crate::pipeline;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::log::{StageLogger, Verbosity};
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

    let mut config =
        pipeline::load_config(&pipeline::find_config(opts.config_override.as_deref())?)?;
    helpers::auto_detect_github(&mut config, &log);

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        token: opts.token,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    helpers::setup_env(&mut ctx, &config, &log)?;
    helpers::resolve_git_context(&mut ctx, &config, &log);

    // Load artifacts from dist/
    let dist = opts.dist.as_deref().unwrap_or(&config.dist);
    helpers::load_artifacts_from_dist(&mut ctx, dist)?;

    log.status(&format!(
        "loaded {} artifact(s) from {}",
        ctx.artifacts.all().len(),
        dist.display()
    ));

    // Run publish-only pipeline
    let p = pipeline::build_publish_pipeline();
    p.run(&mut ctx, &log)
}
