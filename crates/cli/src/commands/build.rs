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

    helpers::auto_detect_github(&mut config, &log, true)?;

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
    if output_path.is_some() {
        refuse_ambiguous_output(&config, &ctx_opts.selected_crates, has_single_target)?;
    }

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
            output_path: output_path.as_deref(),
        },
        &log,
    );
    helpers::finish_with_always_hooks(&ctx, outcome, &log)
}

/// `--output` copies ONE binary, so it is refused before anything compiles
/// when the selection would build more than one: the build entries the stage
/// itself would compile (`crate_build_target_entries`: producing, not
/// `skip:`ped, with at least one target) are counted over every selected
/// crate, and more than one names each crate so the operator can narrow with
/// `--crate`. A `skip:` template is read without a context here, so only a
/// literal truthy value counts as skipped. A single crate whose one build
/// yields several binaries is only known after the build and is refused there.
fn refuse_ambiguous_output(
    config: &anodizer_core::config::Config,
    selected: &[String],
    has_single_target: bool,
) -> Result<()> {
    if !has_single_target {
        anyhow::bail!("--output requires --single-target (only one binary can be copied)");
    }
    let default_targets = config.effective_default_targets();
    let per_crate: Vec<(&str, usize)> = config
        .selected_crates(selected)
        .into_iter()
        .filter_map(|c| {
            let producing =
                anodizer_core::build_plan::crate_build_target_entries(c, &default_targets, |b| {
                    anodizer_core::build_plan::build_is_skipped(b, |t| Ok(t.to_string()))
                })
                .into_iter()
                .filter(|entry| !entry.targets.is_empty())
                .count();
            (producing > 0).then_some((c.name.as_str(), producing))
        })
        .collect();
    let builds: usize = per_crate.iter().map(|(_, n)| n).sum();
    if builds > 1 {
        anyhow::bail!(
            "--output requires a single build: {builds} builds across {} crate(s) ({}) would \
             produce a binary; narrow the selection with --crate, or keep one builds[] entry",
            per_crate.len(),
            per_crate
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// What the bracket body needs from the command line.
struct BuildBody<'a> {
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

    fn crate_with_builds(name: &str, builds: usize) -> anodizer_core::config::CrateConfig {
        use anodizer_core::config::{BuildConfig, CrateConfig};
        CrateConfig {
            name: name.to_string(),
            builds: Some(
                (0..builds)
                    .map(|i| BuildConfig {
                        binary: Some(format!("{name}{i}")),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn output_is_refused_before_the_build_when_more_than_one_binary_would_be_built() {
        let config = anodizer_core::config::Config {
            crates: vec![crate_with_builds("a", 1), crate_with_builds("b", 2)],
            ..Default::default()
        };

        let msg = refuse_ambiguous_output(&config, &[], true)
            .unwrap_err()
            .to_string();
        assert_eq!(
            msg,
            "--output requires a single build: 3 builds across 2 crate(s) (a, b) would produce \
             a binary; narrow the selection with --crate, or keep one builds[] entry"
        );

        let msg = refuse_ambiguous_output(&config, &["b".to_string()], true)
            .unwrap_err()
            .to_string();
        assert!(
            msg.starts_with("--output requires a single build: 2 builds across 1 crate(s) (b)"),
            "{msg}"
        );

        refuse_ambiguous_output(&config, &["a".to_string()], true)
            .expect("one selected crate with one build is unambiguous");

        let msg = refuse_ambiguous_output(&config, &["a".to_string()], false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            msg,
            "--output requires --single-target (only one binary can be copied)"
        );
    }

    #[test]
    fn output_counts_only_the_builds_the_stage_would_compile() {
        use anodizer_core::config::{BuildConfig, StringOrBool};
        let mut krate = crate_with_builds("a", 3);
        let builds = krate.builds.as_mut().unwrap();
        builds[1].skip = Some(StringOrBool::Bool(true));
        builds[2].targets = Some(vec![]);
        builds.push(BuildConfig {
            binary: None,
            ..Default::default()
        });
        let config = anodizer_core::config::Config {
            crates: vec![krate],
            ..Default::default()
        };

        refuse_ambiguous_output(&config, &[], true).expect(
            "a skipped entry, an entry with no targets and a non-producing entry build nothing",
        );
    }

    /// The refusal quoted on the single-target docs page is the message the
    /// command produces (`.claude/rules/docs-quoted-output-pins.md`).
    #[test]
    fn the_refusal_quoted_in_the_single_target_docs_is_what_the_command_produces() {
        let config = anodizer_core::config::Config {
            crates: vec![crate_with_builds("myapp", 2)],
            ..Default::default()
        };
        let produced = refuse_ambiguous_output(&config, &[], true)
            .unwrap_err()
            .to_string();
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/site/content/docs/builds/single-target.md"
        ))
        .unwrap();
        let quoted: Vec<&str> = page
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("Error "))
            .filter(|line| !line.contains('\u{2026}'))
            .collect();
        assert_eq!(quoted, vec![produced.as_str()]);
    }

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
