//! `anodizer continue` command.
//!
//! Two modes mirroring GoReleaser Pro's `goreleaser continue`:
//!
//! - **`--merge`** — merge artifacts from split-build workers (each worker
//!   wrote `dist/<target>/context.json` via `anodizer release --split`) and
//!   then run post-build stages (sign / checksum / sbom / release / publish
//!   / announce). Equivalent to GR Pro's `goreleaser continue --merge`.
//!
//! - **no flag** — single-host stage-resume: load artifacts from a populated
//!   `dist/` (typically left over from a `release --prepare` run or a
//!   previous release that failed in publish/announce) and run the
//!   publish-only pipeline (release + publish + blob), then run the announce
//!   stage and after-hooks. Equivalent to GR Pro's `goreleaser continue` and
//!   the long-standing flow for "transient publish failure, retry without
//!   re-building".

use super::helpers;
use crate::pipeline;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::{StageLogger, Verbosity};
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
    /// When true, run the merge-mode flow: load every per-target
    /// `context.json` under `dist/`, recombine into a single artifact set,
    /// then run sign / checksum / sbom / release / publish / announce.
    /// When false, run the single-host stage-resume flow: load existing
    /// `dist/` artifacts and re-run publish + announce only.
    pub merge: bool,
}

pub fn run(opts: ContinueOpts) -> Result<()> {
    let log = StageLogger::new(
        "continue",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        token: opts.token,
        merge: opts.merge,
        ..Default::default()
    };

    if opts.merge {
        // Merge-mode does its own per-shard context.json load via run_merge,
        // so init_publish_stage_ctx (which loads dist/artifacts.json) is the
        // wrong prelude here — build the context manually with the shared
        // merge-stage prelude.
        let (config, mut ctx) =
            helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;
        return super::release::run_merge(
            &mut ctx,
            &config,
            &log,
            opts.dry_run,
            opts.dist.as_deref(),
        );
    }

    // Single-host stage-resume: load artifacts from dist/ and run the
    // publish-only pipeline (release + publish + blob), matching the
    // `publish` command's behaviour. This is the GR Pro `goreleaser
    // continue` (no `--merge`) path: a prior release stalled mid-publish
    // (e.g. expired token, transient 5xx) and the user wants to resume
    // without rebuilding.
    let (_config, mut ctx, _dist) = helpers::init_publish_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        opts.dist.as_deref(),
        true,
        &log,
    )?;
    ctx.populate_metadata_var()?;

    let p = pipeline::build_publish_pipeline();
    p.run(&mut ctx, &log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn write_minimal_config(dir: &std::path::Path) {
        fs::write(
            dir.join(".anodizer.yaml"),
            r#"project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#,
        )
        .unwrap();
    }

    /// continue (no --merge) follows the single-host stage-resume path:
    /// load dist/ + run publish-only. `init_publish_stage_ctx` runs
    /// `setup_context` (git resolution) before `load_artifacts_from_dist`,
    /// so either failure mode is acceptable — both pin the prelude.
    #[test]
    #[serial]
    fn no_merge_missing_dist_or_git_bails() {
        let tmp = tempfile::tempdir().unwrap();
        write_minimal_config(tmp.path());
        let dist = tmp.path().join("dist-empty");
        fs::create_dir_all(&dist).unwrap();
        let result = run(ContinueOpts {
            dist: Some(dist),
            dry_run: true,
            skip: vec![],
            token: None,
            config_override: Some(tmp.path().join(".anodizer.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
            merge: false,
        });
        assert!(result.is_err(), "must fail with no manifest / no git");
    }

    /// continue --merge takes a different path (run_merge); the prelude
    /// builds context manually but still loads config. A bogus override
    /// must err on find_config.
    #[test]
    #[serial]
    fn merge_missing_config_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(ContinueOpts {
            dist: None,
            dry_run: true,
            skip: vec![],
            token: None,
            config_override: Some(tmp.path().join("nope.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
            merge: true,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    #[test]
    fn continue_opts_struct_round_trips() {
        let opts = ContinueOpts {
            dist: Some(std::path::PathBuf::from("/tmp/x")),
            dry_run: true,
            skip: vec!["docker".into()],
            token: Some("t".into()),
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            merge: false,
        };
        assert!(opts.dry_run);
        assert_eq!(opts.skip, vec!["docker".to_string()]);
        assert_eq!(opts.token.as_deref(), Some("t"));
        assert!(!opts.merge);
    }
}
