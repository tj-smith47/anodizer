use super::helpers;
use crate::pipeline;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result};
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
    pub output: Option<PathBuf>,
    pub skip: Vec<String>,
}

pub fn run(opts: BuildOpts) -> Result<()> {
    let log = StageLogger::new(
        "build",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let config_path = pipeline::find_config(opts.config_override.as_deref())?;
    let (mut config, deprecations) = pipeline::load_config_with_deprecations(&config_path)?;

    // Resolve workspace if specified
    if let Some(ref ws_name) = opts.workspace {
        let ws = super::release::resolve_workspace(&config, ws_name)?.clone();
        helpers::apply_workspace_overlay(&mut config, &ws);
    }

    // Auto-infer project_name from Cargo.toml when not set in config.
    helpers::infer_project_name(&mut config, &log);

    // Auto-detect GitHub owner/name from git remote
    helpers::auto_detect_github(&mut config, &log);

    log.status("building (snapshot)");

    let has_single_target = opts.single_target.is_some();
    let output_path = opts.output;

    let ctx_opts = ContextOptions {
        snapshot: true, // build command always runs in snapshot mode
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        selected_crates: opts.crate_names,
        parallelism: opts.parallelism,
        single_target: opts.single_target,
        skip_stages: opts.skip,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    for (prop, msg) in &deprecations {
        ctx.deprecate(prop, msg);
    }
    helpers::setup_context(&mut ctx, &config, &log)?;

    // Run before-hooks (GoReleaser's BuildCmdPipeline includes before.Pipe).
    // Respect --skip=before like the release pipeline.
    if !ctx.should_skip("before")
        && let Some(before) = &config.before
        && let Some(ref hooks) = before.hooks
    {
        pipeline::run_hooks(hooks, "before", false, &log, Some(ctx.template_vars()))?;
    }

    // Dump effective (resolved) config to dist/config.yaml before the build runs.
    helpers::write_effective_config(&config, &log)?;

    // Run build stage
    let build_stage = anodize_stage_build::BuildStage;
    log.verbose("running build stage");
    build_stage.run(&mut ctx)?;

    // Run UPX stage (compresses binaries if configured)
    let upx_stage = anodize_stage_upx::UpxStage;
    log.verbose("running upx stage");
    upx_stage.run(&mut ctx)?;

    // Binary-only signing (GoReleaser BuildCmdPipeline: sign.BinaryPipe).
    // Mirrors the full release pipeline but skips the generic `signs`
    // loop — at build time only binaries exist, and running `signs` would
    // break user expectations (`signs: [{artifacts: all}]` means "sign
    // everything at release time", not "sign binaries at build time").
    if !ctx.should_skip("sign") {
        let binary_sign_stage = anodize_stage_sign::BinarySignStage;
        log.verbose("running binary-sign stage");
        binary_sign_stage.run(&mut ctx)?;
    }

    // macOS notarization (GoReleaser BuildCmdPipeline: notary.MacOS).
    if !ctx.should_skip("notarize") {
        let notarize_stage = anodize_stage_notarize::NotarizeStage;
        log.verbose("running notarize stage");
        notarize_stage.run(&mut ctx)?;
    }

    // Print artifact size table if configured
    helpers::run_report_sizes(&mut ctx, &config, &log);

    // Write metadata.json + artifacts.json (GoReleaser's BuildCmdPipeline
    // includes metadata.Pipe).
    helpers::write_metadata_and_artifacts(&mut ctx, &config, &log)?;

    // --output: copy the built binary to the specified path
    if let Some(ref output_path) = output_path {
        if !has_single_target {
            anyhow::bail!("--output requires --single-target (only one binary can be copied)");
        }

        // Find the single binary artifact
        let binaries: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == anodize_core::artifact::ArtifactKind::Binary)
            .collect();

        if binaries.is_empty() {
            anyhow::bail!("--output: no binary artifacts found after build");
        }
        if binaries.len() > 1 {
            anyhow::bail!(
                "--output: found {} binary artifacts; use --crate to select a single crate",
                binaries.len()
            );
        }

        let binary = &binaries[0];
        let dest = if output_path.to_string_lossy() == "." {
            // "." means use the binary's filename in the current directory
            PathBuf::from(
                binary
                    .path
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("binary has no filename"))?,
            )
        } else {
            output_path.clone()
        };

        std::fs::copy(&binary.path, &dest).with_context(|| {
            format!(
                "failed to copy binary from {} to {}",
                binary.path.display(),
                dest.display()
            )
        })?;
        log.status(&format!("copied binary to {}", dest.display()));
    }

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
            output: None,
            skip: vec![],
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
            output: None,
            skip: vec![],
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
            output: None,
            skip: vec![],
        };
        assert_eq!(opts.workspace.as_deref(), Some("frontend"));
    }
}
