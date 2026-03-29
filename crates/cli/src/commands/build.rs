use super::helpers;
use crate::pipeline;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::stage::Stage;
use anyhow::Result;
use std::path::PathBuf;

pub struct BuildOpts {
    pub crate_names: Vec<String>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub parallelism: usize,
    pub single_target: Option<String>,
    pub workspace: Option<String>,
}

pub fn run(opts: BuildOpts) -> Result<()> {
    let log = StageLogger::new(
        "build",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let mut config =
        pipeline::load_config(&pipeline::find_config(opts.config_override.as_deref())?)?;

    // Resolve workspace if specified
    if let Some(ref ws_name) = opts.workspace {
        let ws = super::release::resolve_workspace(&config, ws_name)?.clone();
        helpers::apply_workspace_overlay(&mut config, &ws);
    }

    // Auto-detect GitHub owner/name from git remote
    helpers::auto_detect_github(&mut config, &log);

    log.status("building (snapshot)");

    let ctx_opts = ContextOptions {
        snapshot: true, // build command always runs in snapshot mode
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        selected_crates: opts.crate_names,
        parallelism: opts.parallelism,
        single_target: opts.single_target,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();

    // Populate user-defined env vars
    helpers::setup_env(&mut ctx, &config, &log)?;

    // Resolve git info
    helpers::resolve_git_context(&mut ctx, &config, &log);

    // Run build stage
    let build_stage = anodize_stage_build::BuildStage;
    log.verbose("running build stage");
    build_stage.run(&mut ctx)?;

    // Run UPX stage (compresses binaries if configured)
    let upx_stage = anodize_stage_upx::UpxStage;
    log.verbose("running upx stage");
    upx_stage.run(&mut ctx)?;

    log.status("build complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_opts_defaults() {
        let opts = BuildOpts {
            crate_names: vec![],

            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            parallelism: 4,
            single_target: None,
            workspace: None,
        };
        assert_eq!(opts.parallelism, 4);
        assert!(opts.single_target.is_none());
        assert!(opts.workspace.is_none());
    }

    #[test]
    fn test_build_opts_with_single_target() {
        let opts = BuildOpts {
            crate_names: vec!["myapp".to_string()],

            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            parallelism: 2,
            single_target: Some("x86_64-unknown-linux-gnu".to_string()),
            workspace: None,
        };
        assert_eq!(
            opts.single_target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn test_build_opts_with_workspace() {
        let opts = BuildOpts {
            crate_names: vec![],

            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            parallelism: 4,
            single_target: None,
            workspace: Some("frontend".to_string()),
        };
        assert_eq!(opts.workspace.as_deref(), Some("frontend"));
    }
}
