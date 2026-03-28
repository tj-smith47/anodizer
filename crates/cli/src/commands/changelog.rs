use crate::pipeline;
use anodize_core::context::{Context, ContextOptions};
use anodize_core::git;
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::stage::Stage;
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

    let path = pipeline::find_config(config_override)?;
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
    ctx.populate_time_vars();

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

    // Run the changelog stage
    let stage = anodize_stage_changelog::ChangelogStage;
    stage.run(&mut ctx)?;

    // Print changelogs to stdout
    for (crate_name, changelog) in &ctx.changelogs {
        log.verbose(&format!("changelog for '{}'", crate_name));
        println!("{}", changelog);
    }

    if ctx.changelogs.is_empty() {
        log.warn("no changelogs generated");
    }

    Ok(())
}
