//! `anodizer publish` command.
//! Runs only the publish stages (release, publish, blob) from a completed dist/.
//! Equivalent to GoReleaser Pro's `goreleaser publish`.

use super::helpers;
use crate::pipeline;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use std::path::PathBuf;

pub struct PublishOpts {
    pub dry_run: bool,
    pub token: Option<String>,
    pub dist: Option<PathBuf>,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

pub fn run(opts: PublishOpts) -> Result<()> {
    let log = StageLogger::new(
        "publish",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        token: opts.token,
        ..Default::default()
    };
    let (_config, mut ctx, _dist) = helpers::init_publish_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        opts.dist.as_deref(),
        false,
        &log,
    )?;

    // Run publish-only pipeline
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

    #[test]
    #[serial]
    fn missing_config_bails_with_helpful_message() {
        let tmp = tempfile::tempdir().unwrap();
        // No config file, no Cargo.toml -> find_config bails.
        let err = run(PublishOpts {
            dry_run: true,
            token: None,
            dist: None,
            config_override: Some(tmp.path().join("does-not-exist.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("config file not found"),
            "expected missing-config error: {err}"
        );
    }

    /// publish_cmd's prelude is `init_publish_stage_ctx`, which calls
    /// `setup_context` (git resolution) BEFORE `load_artifacts_from_dist`.
    /// Outside a git repo the git step can fail before we reach the
    /// artifact-load step, so either failure mode is acceptable — both
    /// pin the dispatch wiring through the prelude.
    #[test]
    #[serial]
    fn missing_dist_artifacts_or_git_bails() {
        let tmp = tempfile::tempdir().unwrap();
        write_minimal_config(tmp.path());
        let dist = tmp.path().join("dist-empty");
        fs::create_dir_all(&dist).unwrap();
        let result = run(PublishOpts {
            dry_run: true,
            token: None,
            dist: Some(dist),
            config_override: Some(tmp.path().join(".anodizer.yaml")),
            verbose: false,
            debug: false,
            quiet: true,
        });
        assert!(result.is_err(), "must fail with no manifest / no git");
    }

    #[test]
    fn publish_opts_struct_fields_round_trip() {
        // Constructor coverage: ensures the opt struct is wired and the
        // defaults pattern compiles.
        let opts = PublishOpts {
            dry_run: true,
            token: Some("tok".into()),
            dist: None,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
        };
        assert!(opts.dry_run);
        assert!(opts.quiet);
        assert_eq!(opts.token.as_deref(), Some("tok"));
    }
}
