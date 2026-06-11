//! `anodizer preflight` — config-derived environment preflight.
//!
//! Walks the resolved config and asks every enabled surface what it needs
//! from the runner environment — CLI tools, env vars/secrets (presence
//! only, values are never echoed), endpoint reachability, docker daemon,
//! loadable key material — then evaluates everything in one collect-all
//! pass so the operator sees the complete fix list at once.
//!
//! Two consumers share this engine:
//! * the standalone `anodizer preflight` command (CI canary / local check);
//! * `anodizer release` (including `--publish-only`), which runs the same
//!   evaluation before any stage and aborts before side effects when
//!   anything is missing.
//!
//! Requirements are declared next to the code that consumes them — each
//! stage crate exports `env_requirements(ctx)` and each publisher
//! implements `Publisher::requirements` — so the preflight surface cannot
//! drift from what the stages actually read.

use std::path::PathBuf;
use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::env_preflight::{self, EnvPreflightReport, EnvProbes, SourcedRequirement};
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;

/// Which pipeline shape the preflight guards.
///
/// `--publish-only` consumes a preserved dist: artifact-producing stages
/// (build, nfpm, srpm, snapcraft pack, sbom, makeself, upx, appimage)
/// never run there, so demanding their tools would falsely block a
/// publish on a runner that only carries publish-time dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightScope {
    /// Full `anodizer release` pipeline: every configured stage.
    Full,
    /// `anodizer release --publish-only`: sign/checksum/release/docker/
    /// publish/blob/snapcraft-publish/verify-release only (mirrors
    /// `build_publish_only_pipeline`).
    PublishOnly,
}

impl PreflightScope {
    /// Whether `stage` participates in a pipeline of this scope.
    fn includes(self, stage: &str) -> bool {
        match self {
            PreflightScope::Full => true,
            PreflightScope::PublishOnly => matches!(
                stage,
                "sign"
                    | "docker"
                    | "docker-sign"
                    | "release"
                    | "publish"
                    | "blob"
                    | "snapcraft-publish"
                    | "verify-release"
            ),
        }
    }
}

/// Collect every environment requirement the resolved config implies,
/// honoring `--skip` stage selection and the pipeline scope. Per-crate
/// workspace configs union across all publishable crates: the
/// stage/publisher derivations walk the full crate universe, so one pass
/// covers single-crate, lockstep, and per-crate modes alike.
pub fn collect_requirements(ctx: &Context, scope: PreflightScope) -> Vec<SourcedRequirement> {
    let mut out: Vec<SourcedRequirement> = Vec::new();
    let mut add = |source: &str, reqs: Vec<anodizer_core::EnvRequirement>| {
        out.extend(reqs.into_iter().map(|r| SourcedRequirement::new(source, r)));
    };
    let runs = |stage: &str| -> bool { scope.includes(stage) && !ctx.should_skip(stage) };

    // Build stage: the cargo invocation itself (honors the standard CARGO
    // override the build stage resolves).
    if runs("build") {
        let cargo = ctx
            .env_var("CARGO")
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "cargo".to_string());
        add(
            "stage:build",
            vec![anodizer_core::EnvRequirement::Tool { name: cargo }],
        );
    }

    if runs("nfpm") {
        add("stage:nfpm", anodizer_stage_nfpm::env_requirements(ctx));
    }
    if runs("srpm") {
        add("stage:srpm", anodizer_stage_srpm::env_requirements(ctx));
    }
    if runs("snapcraft") {
        add(
            "stage:snapcraft",
            anodizer_stage_snapcraft::build_env_requirements(ctx),
        );
    }
    if runs("snapcraft-publish") {
        add(
            "stage:snapcraft-publish",
            anodizer_stage_snapcraft::publish_env_requirements(ctx),
        );
    }
    // The release pipeline's sign stage drives both `signs:` and
    // `binary_signs:` (BinarySignStage is the `anodizer build` selection),
    // so both slices hang off the `sign` skip gate here.
    if runs("sign") {
        add(
            "stage:sign",
            anodizer_stage_sign::sign_env_requirements(ctx),
        );
        add(
            "stage:sign",
            anodizer_stage_sign::binary_sign_env_requirements(ctx),
        );
    }
    if runs("docker-sign") {
        add(
            "stage:docker-sign",
            anodizer_stage_sign::docker_sign_env_requirements(ctx),
        );
    }
    if runs("sbom") {
        add("stage:sbom", anodizer_stage_sbom::env_requirements(ctx));
    }
    if runs("makeself") {
        add(
            "stage:makeself",
            anodizer_stage_makeself::env_requirements(ctx),
        );
    }
    if runs("upx") {
        add("stage:upx", anodizer_stage_upx::env_requirements(ctx));
    }
    if runs("appimage") {
        add(
            "stage:appimage",
            anodizer_stage_appimage::env_requirements(ctx),
        );
    }
    if runs("docker") {
        add("stage:docker", anodizer_stage_docker::env_requirements(ctx));
    }
    if runs("blob") {
        add("stage:blob", anodizer_stage_blob::env_requirements(ctx));
    }
    if runs("verify-release") {
        add(
            "stage:verify-release",
            anodizer_stage_verify_release::env_requirements(ctx),
        );
    }

    // Release stage: GitHub release creation + asset upload authenticate
    // via the github-release publisher's ladder. The publisher is also in
    // the registry below, but the release stage runs even when `publish`
    // is skipped — declare it under its own gate (the evaluator dedups).
    let release_skipped = ctx
        .config
        .release
        .as_ref()
        .and_then(|r| r.skip.as_ref())
        .is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        });
    if runs("release") && !release_skipped {
        add(
            "stage:release",
            anodizer_core::Publisher::requirements(
                &anodizer_stage_release::publisher::GithubReleasePublisher::new(),
                ctx,
            ),
        );
    }

    // The UNGATED publisher list, not `configured_publishers`: requirement
    // derivation must see publishers configured only on workspace crates,
    // which the registry's top-level-crate predicates cannot — each
    // `Publisher::requirements` self-gates on the resolved config instead.
    if runs("publish") {
        for publisher in anodizer_stage_publish::registry::all_publishers() {
            add(
                &format!("publish:{}", publisher.name()),
                publisher.requirements(ctx),
            );
        }
    }

    out
}

/// Evaluate the collected requirements against the real environment: PATH
/// probes via `core::tool_detect`, `docker info` for the daemon, a plain
/// HTTP round-trip for endpoints (any response means reachable), and env
/// lookups that merge `env_files` entries with the process environment.
pub fn evaluate_against_environment(
    ctx: &Context,
    requirements: &[SourcedRequirement],
) -> EnvPreflightReport {
    let env = |name: &str| -> Option<String> {
        ctx.template_vars()
            .all_env()
            .get(name)
            .cloned()
            .or_else(|| ctx.env_var(name))
    };
    let tool =
        |name: &str| -> bool { anodizer_core::tool_detect::tool_available(name).unwrap_or(false) };
    let endpoint = |url: &str| -> std::result::Result<(), String> {
        let target = if url.contains("://") {
            url.to_string()
        } else {
            format!("https://{url}")
        };
        let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
            .map_err(|e| format!("{e:#}"))?;
        // Any HTTP response — including 403/404 — proves the endpoint is
        // reachable; only transport-level failures count as unreachable.
        client
            .get(&target)
            .send()
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    };
    let docker = || anodizer_core::tool_detect::tool_runs_with_args("docker", &["info"]);
    env_preflight::evaluate(
        requirements,
        &env,
        &EnvProbes {
            tool: &tool,
            endpoint: &endpoint,
            docker: &docker,
        },
    )
}

/// Run the environment preflight for a release pipeline: collect, evaluate,
/// and log the full report. Returns the report; the caller decides whether
/// a non-ok report aborts (release) or just exits non-zero (standalone).
pub fn run_env_preflight(
    ctx: &Context,
    scope: PreflightScope,
    log: &StageLogger,
) -> EnvPreflightReport {
    let requirements = collect_requirements(ctx, scope);
    let report = evaluate_against_environment(ctx, &requirements);
    for line in report.to_string().trim_end_matches('\n').lines() {
        if report.ok() {
            log.status(line);
        } else {
            log.error(line);
        }
    }
    report
}

pub struct PreflightOpts {
    pub config_override: Option<PathBuf>,
    pub json: bool,
    pub skip: Vec<String>,
    pub publish_only: bool,
    pub token: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
}

/// Standalone `anodizer preflight`: load the config, derive the full
/// requirement set, probe the environment, and exit non-zero when anything
/// is missing. Same engine the release pipeline runs before any stage.
pub fn run(opts: PreflightOpts) -> Result<()> {
    let log = StageLogger::new(
        "preflight",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );
    // The context here is observational: `dry_run` keeps the shared init
    // from hard-bailing on guards the report itself must carry (a missing
    // GitHub token would otherwise abort init instead of appearing in the
    // collect-all output alongside every other failure), and `snapshot`
    // lets the canary run from any commit — requirement derivation does
    // not depend on HEAD being tagged.
    let ctx_opts = anodizer_core::context::ContextOptions {
        skip_stages: opts.skip.clone(),
        token: opts.token.clone(),
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        dry_run: true,
        snapshot: true,
        ..Default::default()
    };
    let (_config, ctx) =
        super::helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;

    let scope = if opts.publish_only {
        PreflightScope::PublishOnly
    } else {
        PreflightScope::Full
    };
    let requirements = collect_requirements(&ctx, scope);
    let report = evaluate_against_environment(&ctx, &requirements);

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for line in report.to_string().trim_end_matches('\n').lines() {
            log.status(line);
        }
    }
    if !report.ok() {
        anyhow::bail!(
            "preflight: {} environment failure(s) across {} check(s)",
            report.failures.len(),
            report.checks
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::EnvRequirement;
    use anodizer_core::test_helpers::TestContextBuilder;

    fn crate_from_yaml(yaml: &str) -> anodizer_core::config::CrateConfig {
        serde_yaml_ng::from_str(yaml).expect("crate config yaml")
    }

    /// Per-crate workspace union: a publisher configured ONLY on a
    /// workspace crate (invisible to `configured_publishers`' top-level
    /// predicates until the per-crate overlay flattens it) must still
    /// contribute requirements when collection runs once, up front.
    #[test]
    fn collect_requirements_unions_workspace_crates() {
        let top = crate_from_yaml(
            r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
        );
        let ws_crate = crate_from_yaml(
            r#"
name: wscrate
publish:
  aur:
    private_key: "{{ .Env.PF_TEST_AUR_KEY }}"
"#,
        );
        let ws = anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![ws_crate],
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![top])
            .workspaces(vec![ws])
            .build();

        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            reqs.iter().any(|r| r.source == "publish:scoop"),
            "top-level crate's scoop requirements missing: {reqs:?}"
        );
        let aur_key = reqs.iter().any(|r| {
            r.source == "publish:aur"
                && matches!(
                    &r.requirement,
                    EnvRequirement::KeyEnv { var, .. } if var == "PF_TEST_AUR_KEY"
                )
        });
        assert!(
            aur_key,
            "workspace crate's aur key requirement missing: {reqs:?}"
        );
    }

    /// `--skip=publish` must drop every publisher-sourced requirement
    /// while stage-sourced ones survive.
    #[test]
    fn skip_publish_drops_publisher_requirements() {
        let top = crate_from_yaml(
            r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
        );
        let ctx = TestContextBuilder::new()
            .crates(vec![top])
            .skip_stages(vec!["publish".to_string()])
            .build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            !reqs.iter().any(|r| r.source.starts_with("publish:")),
            "publisher requirements survived --skip=publish: {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| r.source == "stage:build"),
            "stage requirements must survive a publish skip: {reqs:?}"
        );
    }

    /// An empty config must not demand publisher credentials: the
    /// ungated `all_publishers` walk relies on every `requirements`
    /// impl self-gating on its own configuration. The one exception is
    /// github-release — release creation is on by default, so its token
    /// ladder is a real requirement even for an empty config.
    #[test]
    fn empty_config_yields_no_publisher_requirements() {
        let ctx = TestContextBuilder::new().build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            !reqs
                .iter()
                .any(|r| r.source.starts_with("publish:") && r.source != "publish:github-release"),
            "unconfigured publishers contributed requirements: {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| r.source == "publish:github-release"),
            "default-on github-release must require its token ladder: {reqs:?}"
        );
    }
}
