use crate::pipeline;
use anodize_core::config::GitHubConfig;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::git;
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::stage::Stage;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct BuildOpts {
    pub crate_names: Vec<String>,
    pub snapshot: bool,
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

    // Load .env files early
    if let Some(ref env_files) = config.env_files {
        anodize_core::config::load_env_files(env_files).map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    // Resolve workspace if specified
    if let Some(ref ws_name) = opts.workspace {
        let ws = super::release::resolve_workspace(&config, ws_name)?.clone();
        config.crates = ws.crates;
        if ws.changelog.is_some() {
            config.changelog = ws.changelog;
        }
        if !ws.signs.is_empty() {
            config.signs = ws.signs;
        }
        if ws.before.is_some() {
            config.before = ws.before;
        }
        if ws.after.is_some() {
            config.after = ws.after;
        }
        if let Some(env_map) = ws.env {
            let merged = config.env.get_or_insert_with(HashMap::new);
            for (k, v) in env_map {
                merged.insert(k, v);
            }
        }
    }

    // Auto-detect GitHub owner/name from git remote
    let detected_github = git::detect_github_repo().ok();
    for crate_cfg in &mut config.crates {
        if let Some(ref mut release) = crate_cfg.release
            && release.github.is_none()
            && let Some((ref owner, ref name)) = detected_github
        {
            release.github = Some(GitHubConfig {
                owner: owner.clone(),
                name: name.clone(),
            });
        }
    }

    // Build always implies snapshot mode unless explicitly overridden
    let snapshot = true; // build command is always snapshot
    log.status(&format!(
        "building{}",
        if opts.snapshot { " (snapshot)" } else { "" }
    ));

    let ctx_opts = ContextOptions {
        snapshot,
        verbose: opts.verbose,
        debug: opts.debug,
        selected_crates: opts.crate_names,
        parallelism: opts.parallelism,
        single_target: opts.single_target,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    ctx.populate_time_vars();

    // Populate user-defined env vars
    if let Some(ref env_map) = config.env {
        for (key, value) in env_map {
            ctx.template_vars_mut().set_env(key, value);
        }
    }

    // Resolve git info (same pattern as release.rs)
    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| config.crates.iter().find(|c| &c.name == name))
        .or_else(|| config.crates.first());

    if let Some(crate_cfg) = first_crate {
        let latest_tag = git::find_latest_tag_matching(&crate_cfg.tag_template)
            .ok()
            .flatten();
        let tag = latest_tag.clone().unwrap_or_else(|| "v0.0.0".to_string());

        match git::detect_git_info(&tag) {
            Ok(mut git_info) => {
                git_info.previous_tag = latest_tag;
                ctx.git_info = Some(git_info);
                ctx.populate_git_vars();
            }
            Err(e) => {
                log.warn(&format!("could not detect git info: {e}"));
                ctx.populate_git_vars();
            }
        }
    } else {
        ctx.populate_git_vars();
    }

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
            snapshot: false,
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
            snapshot: false,
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
            snapshot: false,
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
