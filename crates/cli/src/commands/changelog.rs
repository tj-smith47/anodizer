use super::helpers;
use crate::pipeline;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use anyhow::Result;
use std::path::Path;

pub fn run(
    crate_name: Option<String>,
    config_override: Option<&Path>,
    verbose: bool,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let log = StageLogger::new("changelog", Verbosity::from_flags(quiet, verbose, debug));

    let path = pipeline::find_config_with_logger(config_override, Some(&log))?;
    let config = pipeline::load_config(&path)?;

    log.status("generating changelog");

    let selected_crates: Vec<String> = if let Some(name) = crate_name {
        vec![name]
    } else {
        Vec::new()
    };

    let ctx_opts = ContextOptions {
        verbose,
        debug,
        selected_crates,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();

    // Resolve git info (shared with release.rs and build.rs)
    helpers::resolve_git_context(&mut ctx, &config, &log)?;

    // Run the changelog stage
    let stage = anodizer_stage_changelog::ChangelogStage;
    stage.run(&mut ctx)?;

    // Print changelogs to stdout
    for (crate_name, changelog) in &ctx.stage_outputs.changelogs {
        log.verbose(&format!("changelog for '{}'", crate_name));
        println!("{}", changelog);
    }

    if ctx.stage_outputs.changelogs.is_empty() {
        log.warn("no changelogs generated");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn missing_config_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let err = run(None, Some(&bogus), false, false, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    /// With a crate filter, the selected_crates list is populated; verify
    /// the missing-config bail still fires through that branch (i.e. the
    /// crate selection happens after find_config).
    #[test]
    #[serial]
    fn missing_config_with_crate_filter_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let err = run(
            Some("some-crate".to_string()),
            Some(&bogus),
            false,
            false,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    /// Verify the crate_name = None branch (Vec::new() selected_crates path)
    /// compiles and propagates through to the same find_config check.
    #[test]
    #[serial]
    fn crate_name_none_branch_compiles_and_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let err = run(None, Some(&bogus), true, true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("config file not found"), "{err}");
    }
}
