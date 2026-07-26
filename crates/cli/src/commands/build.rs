use super::helpers;
use crate::pipeline;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

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

    let config_path =
        pipeline::find_config_with_logger(opts.config_override.as_deref(), Some(&log))?;
    let mut config = pipeline::load_config_logged(&config_path, &log)?;

    // Apply the workspace scope exactly like the release path: the explicit
    // `--workspace` overlay, or the one inferred from a `--crate` selection
    // that lives in a single workspace (so a member build gets its
    // workspace's env/signs), then hard-reject any selected name absent from
    // the post-overlay universe — every stage filters unknown names to an
    // empty set, which would otherwise be a silent no-op "success".
    let workspace_skip = helpers::apply_workspace_scope(
        &mut config,
        opts.workspace.as_deref(),
        &opts.crate_names,
        &log,
    )?;
    let mut skip_stages = opts.skip;
    helpers::merge_skip_stages(&mut skip_stages, &workspace_skip);

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
        skip_stages,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::setup_context(&mut ctx, &config, &log)?;

    // The run enters the `before:`/`always:` bracket here: from this call on,
    // every exit — including a `before:` hook that failed — leaves through the
    // root `always:` hooks. Everything above it (config load, workspace scope,
    // context setup) aborts with zero mutations and without having run a single
    // operator command, so there is nothing for a teardown hook to undo.
    let outcome = run_inside_always_bracket(
        &mut ctx,
        &config,
        BuildBody {
            has_single_target,
            output_path: output_path.as_deref(),
        },
        &log,
    );
    helpers::finish_with_always_hooks(&ctx, outcome, &log)
}

/// The `--output` copy's two inputs, bundled so the bracket body keeps a
/// named-field signature instead of a positional `bool` + `Option` pair.
struct BuildBody<'a> {
    /// `--single-target` was given; `--output` requires it because only one
    /// binary can be copied.
    has_single_target: bool,
    /// `--output <path>`: where to copy the single built binary.
    output_path: Option<&'a Path>,
}

/// Everything `anodizer build` does inside the root `before:`/`always:`
/// bracket: the `before:` hooks, the build / upx / sign / notarize stages,
/// the metadata write, the `--output` copy, and the `after:` hooks.
///
/// Split out of [`run`] so the bracket has one entry point — a step added
/// here is covered by the root `always:` hooks automatically, and one added
/// above the call site is deliberately outside them. Mirrors
/// `release::run_setup_inside_always_bracket`.
fn run_inside_always_bracket(
    ctx: &mut Context,
    config: &anodizer_core::config::Config,
    body: BuildBody<'_>,
    log: &StageLogger,
) -> Result<()> {
    helpers::run_root_before_hooks(ctx, config, false, log)?;

    // Dump effective (resolved) config to dist/config.yaml before the build runs.
    helpers::write_effective_config(config, log)?;

    // Run build stage
    let build_stage = anodizer_stage_build::BuildStage;
    log.verbose("running build stage");
    build_stage.run(ctx)?;

    // Run UPX stage (compresses binaries if configured)
    let upx_stage = anodizer_stage_upx::UpxStage;
    log.verbose("running upx stage");
    upx_stage.run(ctx)?;

    // Binary-only signing.
    // Mirrors the full release pipeline but skips the generic `signs`
    // loop — at build time only binaries exist, and running `signs` would
    // break user expectations (`signs: [{artifacts: all}]` means "sign
    // everything at release time", not "sign binaries at build time").
    if !ctx.should_skip("sign") {
        let binary_sign_stage = anodizer_stage_sign::BinarySignStage;
        log.verbose("running binary-sign stage");
        binary_sign_stage.run(ctx)?;
    }

    // macOS notarization.
    if !ctx.should_skip("notarize") {
        let notarize_stage = anodizer_stage_notarize::NotarizeStage;
        log.verbose("running notarize stage");
        notarize_stage.run(ctx)?;
    }

    // Print artifact size table if configured
    helpers::run_report_sizes(ctx, config, log);

    // Write metadata.json + artifacts.json (the build-command pipeline
    // includes metadata.Pipe).
    helpers::write_metadata_and_artifacts(ctx, config, log)?;

    // --output: copy the built binary to the specified path
    if let Some(output_path) = body.output_path {
        if !body.has_single_target {
            anyhow::bail!("--output requires --single-target (only one binary can be copied)");
        }

        // Find the single binary artifact
        let binaries: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == anodizer_core::artifact::ArtifactKind::Binary)
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
            output_path.to_path_buf()
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

    // The root `after:` hooks are the success lane: they close the run once
    // every artifact the command produces exists (including the `--output`
    // copy). A failed build short-circuits above and reaches only `always:`.
    helpers::run_root_after_hooks(ctx, config, false, log)?;

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
