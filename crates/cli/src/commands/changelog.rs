use super::helpers;
use crate::pipeline;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// Options for `anodizer changelog`.
///
/// Bundled struct rather than positional args so adding a flag at the CLI
/// layer doesn't ripple through every test. Mirrors the shape of
/// `release::ReleaseOpts` / `tag::TagOpts`.
pub struct ChangelogOpts {
    pub crate_name: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub output: Option<PathBuf>,
    pub snapshot: bool,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

pub fn run(opts: ChangelogOpts) -> Result<()> {
    let ChangelogOpts {
        crate_name,
        from,
        to,
        output,
        snapshot,
        config_override,
        verbose,
        debug,
        quiet,
    } = opts;
    let log = StageLogger::new("changelog", Verbosity::from_flags(quiet, verbose, debug));

    let path = pipeline::find_config_with_logger(config_override.as_deref(), Some(&log))?;
    let mut config = pipeline::load_config(&path)?;

    log.status("generating changelog");

    let selected_crates: Vec<String> = match crate_name.as_ref() {
        Some(name) => vec![name.clone()],
        None => Vec::new(),
    };

    // Apply workspace overlay when --crate resolves to a workspace crate so
    // monorepo configs (top-level `workspaces:` rather than `crates:`) hand
    // the changelog stage the right per-crate context. Without this the
    // stage iterates `config.crates`, which is empty for workspace-only
    // configs, and emits nothing.
    if let Some(ref target) = crate_name
        && config.crates.is_empty()
    {
        let ws_for_target = config
            .workspaces
            .as_ref()
            .and_then(|ws_list| {
                ws_list
                    .iter()
                    .find(|ws| ws.crates.iter().any(|c| &c.name == target))
            })
            .cloned();
        if let Some(ws) = ws_for_target {
            log.verbose(&format!(
                "--crate {} lives in workspace '{}'; applying workspace overlay",
                target, ws.name
            ));
            helpers::apply_workspace_overlay(&mut config, &ws);
        }
    }

    let ctx_opts = ContextOptions {
        verbose,
        debug,
        selected_crates,
        snapshot,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    ctx.populate_rustc_vars();

    // Resolve git info (shared with release.rs and build.rs)
    helpers::resolve_git_context(&mut ctx, &config, &log)?;

    // Apply --from / --to overrides AFTER `resolve_git_context` has filled
    // the default `Tag` / `PreviousTag` so the user override wins. `--to`
    // becomes `Tag` (the upper bound of the range) and `--from` becomes
    // `PreviousTag` (the lower bound — matches the changelog stage's
    // `find_latest_tag_matching_with_prefix` semantics).
    if let Some(ref t) = to {
        ctx.template_vars_mut().set("Tag", t);
    }
    if let Some(ref f) = from {
        ctx.template_vars_mut().set("PreviousTag", f);
    }

    // Run the changelog stage
    let stage = anodizer_stage_changelog::ChangelogStage;
    stage.run(&mut ctx)?;

    // Stable iteration order (HashMap iteration is non-deterministic, which
    // makes the stdout output flicker across runs and breaks test pinning).
    let mut entries: Vec<(&String, &String)> = ctx.stage_outputs.changelogs.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    // Aggregate the per-crate changelogs for both stdout printing AND
    // (when --output is set) file writing. Separator between crates so
    // multi-crate output isn't a wall of unattributed bullets.
    let mut aggregated = String::new();
    for (name, body) in &entries {
        if entries.len() > 1 {
            // Multi-crate separator is unconditional (independent of
            // --verbose) so the body of each crate's changelog stays
            // attributable in the combined output. Matches the shape
            // GR uses for monorepo `goreleaser changelog`.
            aggregated.push_str(&format!("\n---\n{}\n---\n\n", name));
        }
        aggregated.push_str(body);
        if !body.ends_with('\n') {
            aggregated.push('\n');
        }
    }

    print!("{}", aggregated);

    if let Some(out_path) = output.as_ref() {
        write_output_file(out_path, &aggregated)?;
        log.status(&format!("wrote {}", out_path.display()));
    }

    if ctx.stage_outputs.changelogs.is_empty() {
        log.warn("no changelogs generated");
    }

    Ok(())
}

fn write_output_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("changelog: create parent dir for {}", path.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("changelog: write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn default_opts(config: Option<&Path>) -> ChangelogOpts {
        ChangelogOpts {
            crate_name: None,
            from: None,
            to: None,
            output: None,
            snapshot: false,
            config_override: config.map(|p| p.to_path_buf()),
            verbose: false,
            debug: false,
            quiet: true,
        }
    }

    #[test]
    #[serial]
    fn missing_config_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let err = run(default_opts(Some(&bogus))).unwrap_err().to_string();
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
        let mut opts = default_opts(Some(&bogus));
        opts.crate_name = Some("some-crate".to_string());
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    /// Verify the crate_name = None branch (Vec::new() selected_crates path)
    /// compiles and propagates through to the same find_config check.
    #[test]
    #[serial]
    fn crate_name_none_branch_compiles_and_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let mut opts = default_opts(Some(&bogus));
        opts.verbose = true;
        opts.debug = true;
        opts.quiet = false;
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    /// `--from`/`--to`/`--snapshot`/`--output` all flow through the new
    /// struct without affecting the bail path. Regression guard for the
    /// CLI-flag plumbing.
    #[test]
    #[serial]
    fn extended_flags_compile_and_bail() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let mut opts = default_opts(Some(&bogus));
        opts.from = Some("v0.1.0".into());
        opts.to = Some("HEAD".into());
        opts.snapshot = true;
        opts.output = Some(tmp.path().join("notes.md"));
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    #[test]
    fn write_output_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c/notes.md");
        write_output_file(&nested, "hello\n").unwrap();
        let read = std::fs::read_to_string(&nested).unwrap();
        assert_eq!(read, "hello\n");
    }
}
