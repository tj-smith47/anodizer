//! `anodizer announce` command.
//! Runs only the announce stage from a completed dist/.
//! Equivalent to GoReleaser Pro's `goreleaser announce` (and `announce --merge`).

use super::helpers;
use crate::pipeline;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use std::path::PathBuf;

pub struct AnnounceOpts {
    pub dry_run: bool,
    pub dist: Option<PathBuf>,
    pub token: Option<String>,
    pub skip: Vec<String>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    /// When true, load `dist/<subdir>/context.json` shards (instead of
    /// `dist/artifacts.json`) before running the announce-only pipeline.
    /// Mirrors GR Pro's `goreleaser announce --merge`.
    pub merge: bool,
}

pub fn run(opts: AnnounceOpts) -> Result<()> {
    let log = StageLogger::new(
        "announce",
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
        // Merge-mode prelude builds the context manually so the per-shard
        // loader can populate it from `dist/<subdir>/context.json` files.
        let (config, mut ctx) =
            helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;

        let dist = opts.dist.as_deref().unwrap_or(&config.dist).to_path_buf();
        super::release::load_split_contexts_into(&mut ctx, &dist, &log)?;

        let p = pipeline::build_announce_pipeline();
        return p.run(&mut ctx, &log);
    }

    let (_config, mut ctx, _dist) = helpers::init_publish_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        opts.dist.as_deref(),
        false,
        &log,
    )?;

    let p = pipeline::build_announce_pipeline();
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

    #[test]
    #[serial]
    fn missing_config_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(AnnounceOpts {
            dry_run: true,
            dist: None,
            token: None,
            skip: vec![],
            config_override: Some(tmp.path().join("nope.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
            merge: false,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    /// `init_publish_stage_ctx` calls `setup_context` (git resolution)
    /// before `load_artifacts_from_dist`; either failure mode is
    /// acceptable — both pin the prelude wiring.
    #[test]
    #[serial]
    fn missing_dist_or_git_bails() {
        let tmp = tempfile::tempdir().unwrap();
        write_minimal_config(tmp.path());
        let dist = tmp.path().join("dist-empty");
        fs::create_dir_all(&dist).unwrap();
        let result = run(AnnounceOpts {
            dry_run: true,
            dist: Some(dist),
            token: None,
            skip: vec![],
            config_override: Some(tmp.path().join(".anodizer.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
            merge: false,
        });
        assert!(result.is_err(), "must fail with no manifest / no git");
    }

    /// `announce --merge` reaches `find_config` first; an absent config must
    /// surface as the find-config error, identical to the no-merge path.
    #[test]
    #[serial]
    fn merge_missing_config_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(AnnounceOpts {
            dry_run: true,
            dist: None,
            token: None,
            skip: vec![],
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
    fn announce_opts_skip_stages_wires_through() {
        let opts = AnnounceOpts {
            dry_run: true,
            dist: None,
            token: None,
            skip: vec!["twitter".into(), "discord".into()],
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            merge: false,
        };
        assert_eq!(opts.skip.len(), 2);
        assert_eq!(opts.skip[0], "twitter");
    }

    #[test]
    fn announce_opts_merge_flag_round_trips() {
        let opts = AnnounceOpts {
            dry_run: true,
            dist: None,
            token: None,
            skip: vec![],
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            merge: true,
        };
        assert!(opts.merge);
    }
}
