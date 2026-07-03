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
    /// blob/publish/snapcraft-publish/announce/verify-release only
    /// (mirrors `build_publish_only_pipeline`).
    PublishOnly,
    /// `anodizer release --announce-only`: re-fires announcers against a
    /// prior run's report — announce is the only stage that runs, so only
    /// its requirements apply.
    AnnounceOnly,
    /// `anodizer release --preflight-secrets`: a central pre-tag gate for
    /// decoupled CI runners (build / determinism shards on many hosts plus
    /// a publish runner) that all carry the SAME injected secrets but
    /// different host-local tools. Collects the FULL surface (like
    /// [`PreflightScope::Full`]) then retains only the runner-agnostic
    /// credential requirements — `EnvAllOf` / `EnvAnyOf` / `KeyEnv` — and
    /// drops `Tool` / `ToolAnyOf` / `DockerDaemon` / `Endpoint` / `KeyFile`,
    /// none of which is guaranteed present on the gate runner. Structural
    /// validation still applies to env-borne `KeyEnv` material, so a malformed
    /// secret key is caught up front; on-disk `KeyFile` material is dropped
    /// (the path may not be materialized on the gate runner) and so is not
    /// validated by this scope.
    SecretsOnly,
}

impl PreflightScope {
    /// Whether `stage` participates in a pipeline of this scope.
    fn includes(self, stage: &str) -> bool {
        match self {
            // `SecretsOnly` walks the full stage surface (the credential
            // retention happens in the post-collection `retains` filter);
            // only the env/key-env requirements survive that filter.
            PreflightScope::Full | PreflightScope::SecretsOnly => true,
            PreflightScope::PublishOnly => matches!(
                stage,
                "sign"
                    | "docker"
                    | "docker-sign"
                    | "release"
                    | "publish"
                    | "blob"
                    | "snapcraft-publish"
                    | "announce"
                    | "verify-release"
            ),
            PreflightScope::AnnounceOnly => stage == "announce",
        }
    }

    /// Whether `req` survives this scope's post-collection filter.
    ///
    /// Every scope except [`PreflightScope::SecretsOnly`] keeps the full
    /// requirement set the stage/publisher derivations produced.
    /// `SecretsOnly` retains only the runner-agnostic credential kinds —
    /// the CI secrets injected into every decoupled job — and drops the
    /// host-local kinds (tools, docker daemon, endpoints, and on-disk key
    /// files, which may not be materialized on the gate runner).
    fn retains(self, req: &anodizer_core::EnvRequirement) -> bool {
        use anodizer_core::EnvRequirement::{EnvAllOf, EnvAnyOf, KeyEnv};
        match self {
            PreflightScope::SecretsOnly => {
                matches!(req, EnvAllOf { .. } | EnvAnyOf { .. } | KeyEnv { .. })
            }
            _ => true,
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
    // Stages that self-skip at runtime when a `--publishers` allowlist (or
    // `--skip`) deselects them (docker/docker-sign/blob/snapcraft-publish/
    // announce) must gate their preflight requirements on the SAME predicate
    // the runtime uses, or an npm-only `--publishers npm` run is falsely
    // blocked by cosign/minio/snapcraft tooling those stages will never reach.
    // Mirrors the publish-loop's `publisher_deselected` lockstep below.
    let runs_selected =
        |stage: &str| -> bool { scope.includes(stage) && !ctx.publisher_deselected(stage) };
    // The build stage's preflight is split across two sites — the HARD `cargo`
    // probe here and the ADVISORY cross-toolchain append after the `add`
    // closure's last use (a borrow-checker constraint). Decide once so the two
    // halves can never gate on divergent predicates.
    let build_runs = runs("build");

    // Build stage: the run path spawns the literal `cargo` from PATH, so
    // probe exactly that, then add the cross-compilation toolchain the build
    // resolves per target (cargo-zigbuild + zig, cross, or a system cross gcc)
    // so `anodizer tools` reports what a runner must install to cross-compile
    // instead of the action re-deriving it in bash. The SecretsOnly scope
    // drops every `Tool` requirement (see `retains`), so these surface only in
    // the full requirement set `anodizer tools` reads — never in the pre-tag
    // secrets gate.
    if build_runs {
        // `cargo` is HARD-required: the build literally spawns it from PATH.
        // The cross-compilation toolchain is ADVISORY and appended after the
        // `add`-closure collection below (the build degrades gracefully without
        // it, so a missing zig/cargo-zigbuild must warn, not block the gate).
        add(
            "stage:build",
            vec![anodizer_core::EnvRequirement::Tool {
                name: "cargo".to_string(),
            }],
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
    if runs_selected("snapcraft-publish") {
        add(
            "stage:snapcraft-publish",
            anodizer_stage_snapcraft::publish_env_requirements(ctx),
        );
    }
    // The release pipeline's sign stage drives both `signs:` and
    // `binary_signs:` (BinarySignStage is the `anodizer build` selection),
    // so both slices hang off the `sign` skip gate here. Each slice carries an
    // additional gate that mirrors what `SignStage::run` does at runtime, so
    // preflight cannot demand credentials for work the stage will not perform:
    //
    // - `signs:` (detached archive/checksum signatures) self-skips when EVERY
    //   publisher in `signs_consumers()` is deselected (shared
    //   `signs_fully_deselected` predicate), since no selected surface would
    //   read them — so an npm-only `--publishers npm` run is not demanded
    //   cosign/GPG material for signatures it will never produce.
    //
    // - `binary_signs:` (raw-binary signatures embedded into archives at BUILD
    //   time) self-skips in `--publish-only` mode: its `binary_sign`-marked
    //   output is filtered out of every publish-time consumer
    //   (`is_binary_sign_output`), so in publish-only it is discarded work that
    //   would demand cosign/GPG material the publish-time runner does not carry.
    //   The runtime keys this on `ctx.is_publish_only()` via the shared
    //   `binary_signs_skipped` predicate; here the `PublishOnly` SCOPE is the
    //   authoritative publish-only signal (the standalone `preflight` ctx does
    //   not carry the run-level flag), and both derive from the same
    //   `--publish-only` selection. The full release job (`Full` scope, empty
    //   allowlist) still demands binary-signing material, so binaries that ship
    //   are still signed.
    if runs("sign") {
        if !anodizer_stage_sign::signs_fully_deselected(ctx) {
            add(
                "stage:sign",
                anodizer_stage_sign::sign_env_requirements(ctx),
            );
        }
        if scope != PreflightScope::PublishOnly {
            add(
                "stage:sign",
                anodizer_stage_sign::binary_sign_env_requirements(ctx),
            );
        }
    }
    if runs_selected("docker-sign") {
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
    if runs_selected("docker") {
        add("stage:docker", anodizer_stage_docker::env_requirements(ctx));
    }
    if runs_selected("blob") {
        add("stage:blob", anodizer_stage_blob::env_requirements(ctx));
    }
    if runs("verify-release") {
        add(
            "stage:verify-release",
            anodizer_stage_verify_release::env_requirements(ctx),
        );
    }
    // Per-platform bundler stages: each gates itself on the configured
    // build targets (a darwin-only matrix never demands makensis, a
    // --single-target host release never demands cross-platform tools).
    if runs("msi") {
        add("stage:msi", anodizer_stage_msi::env_requirements(ctx));
    }
    if runs("nsis") {
        add("stage:nsis", anodizer_stage_nsis::env_requirements(ctx));
    }
    if runs("pkg") {
        add("stage:pkg", anodizer_stage_pkg::env_requirements(ctx));
    }
    if runs("dmg") {
        add("stage:dmg", anodizer_stage_dmg::env_requirements(ctx));
    }
    if runs("appbundle") {
        add(
            "stage:appbundle",
            anodizer_stage_appbundle::env_requirements(ctx),
        );
    }
    if runs("flatpak") {
        add(
            "stage:flatpak",
            anodizer_stage_flatpak::env_requirements(ctx),
        );
    }
    if runs("notarize") {
        add(
            "stage:notarize",
            anodizer_stage_notarize::env_requirements(ctx),
        );
    }
    if runs_selected("announce") {
        add(
            "stage:announce",
            anodizer_stage_announce::env_requirements(ctx),
        );
    }

    // Release stage: GitHub release creation + asset upload authenticate
    // via the github-release publisher's ladder. The publisher is also in
    // the registry below, but the release stage runs even when `publish`
    // is skipped — declare it under its own gate (the evaluator dedups).
    //
    // github-release is a real publisher, so the release stage self-skips at
    // runtime when a `--publishers` allowlist deselects it (keyed on the
    // PUBLISHER name `github-release`, the same predicate `ReleaseStage::run`
    // and the publish-loop below use) — gate the preflight demand on that too,
    // so an npm-only `--publishers npm` run is not asked for the GitHub token
    // ladder for a release it will never create. The `--skip` stage gate stays
    // on the STAGE name `release` (`runs("release")`), since `--skip=release`
    // is a stage-name denylist; the publisher-name gate is additive on top of
    // it. An EMPTY allowlist deselects nothing, so the main release job
    // (empty allowlist + `--skip=npm`) still demands the ladder.
    let release_skipped = ctx
        .config
        .release
        .as_ref()
        .and_then(|r| r.skip.as_ref())
        .is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        });
    if runs("release") && !ctx.publisher_deselected("github-release") && !release_skipped {
        add(
            "stage:release",
            anodizer_core::Publisher::requirements(
                &anodizer_stage_release::publisher::GithubReleasePublisher::new(),
                ctx,
            ),
        );
    }

    // The full registry, not `configured_publishers`: requirement derivation
    // must see publishers configured only on workspace crates, which the
    // registry's top-level-crate predicates cannot — each
    // `Publisher::requirements` self-gates on the resolved config instead.
    // Publishers DESELECTED at runtime via `--publishers` (allowlist) or
    // `--skip` (denylist) are excluded so their secrets/tools cannot gate a
    // run that will never invoke them — `publisher_deselected` is the same
    // predicate the publish dispatch layer uses, keeping preflight and
    // dispatch in lockstep across every config mode.
    if runs("publish") {
        for publisher in anodizer_stage_publish::registry::all_publishers() {
            if ctx.publisher_deselected(publisher.name()) {
                continue;
            }
            add(
                &format!("publish:{}", publisher.name()),
                publisher.requirements(ctx),
            );
        }
    }

    // Cross-compilation toolchain — ADVISORY. The build stage resolves a
    // strategy per target and degrades gracefully when the preferred tool is
    // absent (zigbuild → cargo → system gcc; see stage-build's
    // `detect_cross_strategy`), so a missing `zig`/`cargo-zigbuild` must WARN,
    // never block a release. `anodizer tools` still self-reports these as the
    // recommended toolchain. Appended here — after the `add` closure's last use,
    // so it can push to `out` directly — and BEFORE the scope `retain`, so
    // SecretsOnly drops it from the pre-tag gate exactly like every other Tool.
    if build_runs {
        out.extend(
            anodizer_stage_build::cross_tool_requirements(ctx)
                .into_iter()
                .map(|r| SourcedRequirement::new_advisory("stage:build", r)),
        );
    }

    out.retain(|sr| scope.retains(&sr.requirement));
    out
}

/// Evaluate the collected requirements against the real environment: a
/// pure PATH lookup for tools (mirroring how stages spawn them — a tool
/// requirement means "resolvable on PATH", not "answers `--version`",
/// which arbitrary user-configured sign/publish commands may not),
/// `docker info` for the daemon, a plain HTTP round-trip for endpoints
/// (any response means reachable), and env lookups that merge
/// `env_files` entries with the process environment.
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
    let tool = |name: &str| -> bool { anodizer_core::tool_detect::on_path(name) };
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

/// Distinct cosign `env://VAR` key references the collected requirements
/// declare. Each [`anodizer_core::EnvRequirement::KeyEnv`] of kind
/// [`anodizer_core::KeyKind::Cosign`] originates from an `env://VAR` config
/// ref, so the scheme ref is reconstructed exactly as `env://{var}`. Used to
/// drive the offline key-LOAD verification (presence is already checked by the
/// `KeyEnv` evaluation; loadability — decrypting with `COSIGN_PASSWORD` — is
/// not, and that is what a future encrypted-key rotation would silently break).
fn cosign_key_refs(requirements: &[SourcedRequirement]) -> Vec<String> {
    use anodizer_core::EnvRequirement::KeyEnv;
    let mut refs: Vec<String> = Vec::new();
    for sr in requirements {
        if let KeyEnv {
            kind: anodizer_core::KeyKind::Cosign,
            var,
        } = &sr.requirement
        {
            let key_ref = format!("env://{var}");
            if !refs.contains(&key_ref) {
                refs.push(key_ref);
            }
        }
    }
    refs
}

/// Offline-verify every cosign `env://VAR` signing key the config declares: the
/// `KeyEnv` evaluation already proved the secret is PRESENT and structurally a
/// cosign key, but never that it actually LOADS with `COSIGN_PASSWORD`. A future
/// rotation to an ENCRYPTED key with a wrong/empty password would pass both the
/// presence and structure checks, then fail later in the sign stage — after a
/// whole build and determinism run. This runs `cosign public-key --key
/// env://VAR` (local key decrypt, no tlog, no network) in stage-sign before the
/// tag is cut.
///
/// Returns `true` when every declared cosign key loaded (or none were declared),
/// `false` when cosign IS installed and a key failed to load (a genuinely bad
/// secret — a hard preflight failure). When cosign is NOT on PATH the check is
/// not skipped silently: it WARNs that load verification is deferred to sign
/// time, and does NOT fail (the runner simply lacks the tool).
fn verify_cosign_keys_load(requirements: &[SourcedRequirement], log: &StageLogger) -> bool {
    verify_cosign_keys_load_with(
        requirements,
        log,
        anodizer_stage_sign::verify_cosign_key_loads,
    )
}

/// Inner of [`verify_cosign_keys_load`] with the per-ref load resolver injected,
/// so a test can drive the `CosignUnavailable`/`Failed` branches deterministically
/// regardless of whether cosign is on the runner's PATH.
fn verify_cosign_keys_load_with(
    requirements: &[SourcedRequirement],
    log: &StageLogger,
    load: impl Fn(&str) -> anodizer_stage_sign::CosignKeyLoad,
) -> bool {
    let mut all_loaded = true;
    for key_ref in cosign_key_refs(requirements) {
        match load(&key_ref) {
            anodizer_stage_sign::CosignKeyLoad::Loaded => {
                log.status(&format!("cosign key {key_ref} loads (offline verify)"));
            }
            anodizer_stage_sign::CosignKeyLoad::CosignUnavailable => {
                log.warn(&format!(
                    "cosign not installed; skipping offline {key_ref} load verification \
                     — the key/password combo will be validated at sign time instead"
                ));
            }
            anodizer_stage_sign::CosignKeyLoad::CosignProbeFailed(detail) => {
                // A broken probe is NOT a clean "cosign absent": name why the
                // precheck was skipped so an I/O failure isn't masqueraded as a
                // tool-missing skip. Sign time still re-validates, so WARN (not
                // a hard gate failure), mirroring the unavailable case.
                log.warn(&format!(
                    "{detail}; skipping offline {key_ref} load verification \
                     — the key/password combo will be validated at sign time instead"
                ));
            }
            anodizer_stage_sign::CosignKeyLoad::Failed(detail) => {
                all_loaded = false;
                log.error(&format!(
                    "cosign key {key_ref} failed to load (wrong or missing COSIGN_PASSWORD, \
                     or malformed key): {detail}"
                ));
            }
        }
    }
    all_loaded
}

/// Emit each advisory (non-blocking) preflight warning as a `log.warn` line.
/// These are tools the run would PREFER but does not require — the declaring
/// stage degrades gracefully without them (e.g. the build's cross-compile
/// toolchain) — so they surface as a recommendation, never a gate failure.
fn log_preflight_warnings(report: &EnvPreflightReport, log: &StageLogger) {
    for w in &report.warnings {
        log.warn(&format!(
            "{} [recommended by: {}]",
            w.message,
            w.needed_by.join(", ")
        ));
    }
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
    let mut report = evaluate_against_environment(ctx, &requirements);
    for line in report.to_string().trim_end_matches('\n').lines() {
        if report.ok() {
            log.status(line);
        } else {
            log.error(line);
        }
    }
    log_preflight_warnings(&report, log);
    if !verify_cosign_keys_load(&requirements, log) {
        report.note_failure(
            "stage:sign",
            "cosign signing key failed offline load verification",
        );
    }
    report
}

pub struct PreflightOpts {
    pub config_override: Option<PathBuf>,
    pub json: bool,
    pub skip: Vec<String>,
    /// `--publishers` allowlist: mirrors `release --publishers` so the
    /// standalone canary can validate the exact publish-time surface a
    /// publisher-scoped release runs (e.g. the npm-provenance job's
    /// `--publishers npm`), including the stages that self-skip when a
    /// publisher is deselected.
    pub publishers: Vec<String>,
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
        publisher_allowlist: opts.publishers.clone(),
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
    let mut report = evaluate_against_environment(&ctx, &requirements);

    // The offline cosign key-LOAD verification is folded into the report BEFORE
    // it is rendered/serialized, so `--json` consumers and the human report
    // agree on ok/failure and a bad key drives the non-zero exit below. The
    // cosign-absent WARN is emitted to the log regardless of `--json`, since it
    // is advisory (the load is deferred to sign time), not a structured failure.
    if !verify_cosign_keys_load(&requirements, &log) {
        report.note_failure(
            "stage:sign",
            "cosign signing key failed offline load verification",
        );
    }

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for line in report.to_string().trim_end_matches('\n').lines() {
            log.status(line);
        }
        log_preflight_warnings(&report, &log);
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
    /// impl self-gating on its own configuration. That includes
    /// github-release — the release stage only releases crates carrying a
    /// `release:` block (real configs get one injected by defaults
    /// merging), so a crate-less config demands no token.
    #[test]
    fn empty_config_yields_no_publisher_requirements() {
        let ctx = TestContextBuilder::new().build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            !reqs.iter().any(|r| r.source.starts_with("publish:")),
            "unconfigured publishers contributed requirements: {reqs:?}"
        );

        // And the inverse: a crate WITH a release block demands the ladder.
        let top = crate_from_yaml("name: top\nrelease: { github: { owner: o, name: r } }");
        let ctx = TestContextBuilder::new().crates(vec![top]).build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            reqs.iter().any(|r| r.source == "publish:github-release"),
            "a configured release block must require the token ladder: {reqs:?}"
        );
    }

    /// Bundler stages contribute their tools only when the configured
    /// build targets include the platform they package: a Windows target
    /// matrix demands makensis + the WiX toolchain, a Linux-only matrix
    /// demands neither — and dmg's detection ladder surfaces as a
    /// tool-any-of, not three hard requirements.
    #[test]
    fn bundler_requirements_follow_configured_targets() {
        let installer_yaml = |targets: &str| {
            crate_from_yaml(&format!(
                r#"
name: app
builds:
  - binary: app
    targets: [{targets}]
msis:
  - wxs: app.wxs
    version: v4
nsis:
  - script: app.nsi
dmgs:
  - {{}}
flatpaks:
  - app_id: org.example.App
"#
            ))
        };

        let ctx = TestContextBuilder::new()
            .crates(vec![installer_yaml(
                "x86_64-pc-windows-msvc, aarch64-apple-darwin",
            )])
            .build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        let tool = |reqs: &[SourcedRequirement], source: &str, name: &str| {
            reqs.iter().any(|r| {
                r.source == source
                    && matches!(&r.requirement, EnvRequirement::Tool { name: n } if n == name)
            })
        };
        assert!(
            tool(&reqs, "stage:msi", "wix"),
            "windows target must demand the configured WiX v4 toolchain: {reqs:?}"
        );
        assert!(
            tool(&reqs, "stage:nsis", "makensis"),
            "windows target must demand makensis: {reqs:?}"
        );
        let dmg_ladder = reqs.iter().any(|r| {
            r.source == "stage:dmg"
                && matches!(
                    &r.requirement,
                    EnvRequirement::ToolAnyOf { names } if names.contains(&"hdiutil".to_string())
                )
        });
        assert!(
            dmg_ladder,
            "darwin target must demand the dmg tool ladder: {reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| r.source == "stage:flatpak"),
            "no linux target configured — flatpak must contribute nothing: {reqs:?}"
        );

        let ctx = TestContextBuilder::new()
            .crates(vec![installer_yaml("x86_64-unknown-linux-gnu")])
            .build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        for absent in ["stage:msi", "stage:nsis", "stage:dmg", "stage:pkg"] {
            assert!(
                !reqs.iter().any(|r| r.source == absent),
                "linux-only matrix must not demand {absent} tools: {reqs:?}"
            );
        }
        assert!(
            tool(&reqs, "stage:flatpak", "flatpak-builder"),
            "linux target must demand flatpak-builder: {reqs:?}"
        );
    }

    /// An active `notarize.macos` entry demands rcodesign plus the env
    /// refs of its templated secret fields; the templated values
    /// themselves never appear in the requirements.
    #[test]
    fn notarize_requirements_follow_active_entries() {
        use anodizer_core::config::{
            MacOSNotarizeApiConfig, MacOSSignConfig, MacOSSignNotarizeConfig, NotarizeConfig,
        };
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.notarize = Some(NotarizeConfig {
            macos: Some(vec![MacOSSignNotarizeConfig {
                sign: Some(MacOSSignConfig {
                    certificate: Some("{{ .Env.PF_P12_B64 }}".to_string()),
                    password: Some("{{ .Env.PF_P12_PASSWORD }}".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNotarizeApiConfig {
                    key: Some("{{ .Env.PF_ASC_KEY }}".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            reqs.iter().any(|r| {
                r.source == "stage:notarize"
                    && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "rcodesign")
            }),
            "active macos entry must demand rcodesign: {reqs:?}"
        );
        for var in ["PF_P12_B64", "PF_P12_PASSWORD", "PF_ASC_KEY"] {
            assert!(
                reqs.iter().any(|r| {
                    r.source == "stage:notarize"
                        && matches!(
                            &r.requirement,
                            EnvRequirement::EnvAllOf { vars } if vars.contains(&var.to_string())
                        )
                }),
                "templated notarize field must demand {var}: {reqs:?}"
            );
        }
    }

    /// Announce credentials derive from the per-announcer env resolution:
    /// SMTP_PASSWORD for an enabled email announcer, the SLACK_WEBHOOK
    /// fallback for slack without a configured webhook_url, the env ref of
    /// a templated webhook_url instead when one is set — and announce is
    /// part of the publish-only scope (its pipeline runs the stage last).
    #[test]
    fn announce_requirements_derive_from_announcer_config() {
        use anodizer_core::config::{
            AnnounceConfig, EmailAnnounce, SlackAnnounce, StringOrBool, TelegramAnnounce,
        };
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.announce = Some(AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(StringOrBool::Bool(true)),
                host: Some("smtp.example.com".to_string()),
                username: Some("releases@example.com".to_string()),
                ..Default::default()
            }),
            slack: Some(SlackAnnounce {
                enabled: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            telegram: Some(TelegramAnnounce {
                enabled: Some(StringOrBool::Bool(true)),
                bot_token: Some("{{ .Env.PF_TG_TOKEN }}".to_string()),
                ..Default::default()
            }),
            // Present but not enabled: must contribute nothing.
            reddit: Some(Default::default()),
            ..Default::default()
        });

        for scope in [PreflightScope::Full, PreflightScope::PublishOnly] {
            let reqs = collect_requirements(&ctx, scope);
            let announce_env = |var: &str| {
                reqs.iter().any(|r| {
                    r.source == "stage:announce"
                        && matches!(
                            &r.requirement,
                            EnvRequirement::EnvAllOf { vars } if vars.contains(&var.to_string())
                        )
                })
            };
            assert!(
                announce_env("SMTP_PASSWORD"),
                "{scope:?}: enabled email announcer must demand SMTP_PASSWORD: {reqs:?}"
            );
            assert!(
                announce_env("SLACK_WEBHOOK"),
                "{scope:?}: slack without webhook_url must demand the fallback: {reqs:?}"
            );
            assert!(
                announce_env("PF_TG_TOKEN"),
                "{scope:?}: templated bot_token must demand its env ref: {reqs:?}"
            );
            assert!(
                !announce_env("TELEGRAM_TOKEN"),
                "{scope:?}: configured bot_token must not demand the fallback: {reqs:?}"
            );
            assert!(
                !announce_env("REDDIT_SECRET"),
                "{scope:?}: a present-but-disabled announcer must contribute nothing: {reqs:?}"
            );
        }

        // `--skip=announce` drops the whole surface.
        let mut skipped = TestContextBuilder::new()
            .skip_stages(vec!["announce".to_string()])
            .build();
        skipped.config.announce = ctx.config.announce.clone();
        let reqs = collect_requirements(&skipped, PreflightScope::Full);
        assert!(
            !reqs.iter().any(|r| r.source == "stage:announce"),
            "--skip=announce must drop announce requirements: {reqs:?}"
        );
    }

    /// `--publishers npm` must restrict the publish-only preflight to npm's
    /// requirements alone: the github-hosted npm-provenance job carries only
    /// NPM_TOKEN, so demanding cargo / chocolatey / etc. credentials —
    /// publishers the allowlist deselected — falsely aborts the run.
    #[test]
    fn publishers_allowlist_restricts_publisher_requirements() {
        let top = crate_from_yaml(
            r#"
name: top
publish:
  cargo: {}
  scoop:
    repository: { owner: o, name: bucket }
"#,
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![top])
            .publisher_allowlist(vec!["npm".to_string()])
            .build();
        ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
        let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);

        let publisher_sources: Vec<&str> = reqs
            .iter()
            .map(|r| r.source.as_str())
            .filter(|s| s.starts_with("publish:"))
            .collect();
        assert!(
            !publisher_sources.is_empty(),
            "the npm publisher must still contribute its own requirements: {reqs:?}"
        );
        assert!(
            publisher_sources.iter().all(|s| *s == "publish:npm"),
            "--publishers npm must yield only npm publisher requirements: {publisher_sources:?}"
        );
        for absent in ["publish:cargo", "publish:scoop", "publish:github-release"] {
            assert!(
                !reqs.iter().any(|r| r.source == absent),
                "deselected publisher {absent} must contribute no requirements: {reqs:?}"
            );
        }
    }

    /// The github-hosted npm-provenance job runs
    /// `release --publish-only --publishers npm` with NO `--skip`: the
    /// `--publishers npm` allowlist alone must deselect every non-npm surface.
    /// That is every OTHER publisher PLUS the stages that self-skip at runtime
    /// on publisher deselection —
    ///
    /// - publisher-named stages — blob / snapcraft-publish / docker /
    ///   docker-sign / announce / **release** (github-release is a real
    ///   publisher, so the release stage self-skips when it is deselected);
    /// - the `signs:` slice of the `sign` stage — its only consumers are
    ///   github-release / blob / artifactory, ALL deselected here, so the
    ///   stage skips the detached-signature loop and demands no cosign/GPG.
    ///
    /// `binary_signs:` is the negative control: it is the build-time
    /// binary-signing selection (a different consumer model) and is NOT gated
    /// on the publish-time allowlist, so it survives.
    #[test]
    fn publishers_allowlist_deselects_self_skipping_stages() {
        let top = crate_from_yaml(
            r#"
name: top
release: { github: { owner: o, name: r } }
blobs:
  - provider: s3
    bucket: releases
    endpoint: "https://minio.example.com"
snapcrafts:
  - publish: true
publish:
  cargo: {}
"#,
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![top])
            .signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            // binary_signs is gated OFF in PublishOnly scope (its output has no
            // publish-time consumer), so it contributes nothing here regardless
            // of the allowlist.
            .binary_signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .publisher_allowlist(vec!["npm".to_string()])
            .build();
        ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
        ctx.config.announce = Some({
            use anodizer_core::config::{AnnounceConfig, SlackAnnounce, StringOrBool};
            AnnounceConfig {
                slack: Some(SlackAnnounce {
                    enabled: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }
        });

        let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);
        let has = |source: &str| reqs.iter().any(|r| r.source == source);
        // Count the `stage:sign` cosign tool demands. Both slices use cosign,
        // so the COUNT distinguishes "neither slice ran" (0), "one slice ran"
        // (1), and "both ran" (2).
        let cosign_sign_reqs = reqs
            .iter()
            .filter(|r| {
                r.source == "stage:sign"
                    && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
            })
            .count();

        assert!(
            has("publish:npm"),
            "npm is selected — it must still contribute its requirements: {reqs:?}"
        );
        for absent in [
            "stage:blob",
            "stage:snapcraft-publish",
            "stage:docker",
            "stage:docker-sign",
            "stage:announce",
            "stage:release",
        ] {
            assert!(
                !has(absent),
                "allowlist-deselected stage {absent} must contribute no requirements: {reqs:?}"
            );
        }
        // PublishOnly scope: the `signs:` consumers are ALL deselected (npm-only
        // allowlist) so that slice self-skips, AND `binary_signs:` is gated off
        // for publish-only (no publish-time consumer). Neither slice runs, so
        // ZERO cosign demands survive — this is the npm-job's clean surface.
        assert_eq!(
            cosign_sign_reqs, 0,
            "under --publish-only --publishers npm BOTH sign slices must self-skip \
             (signs: all consumers deselected; binary_signs: publish-only): {reqs:?}"
        );

        // Selecting any ONE signs consumer (here: github-release) revives the
        // signs slice; binary_signs stays publish-only-skipped, so exactly ONE
        // cosign demand returns.
        let mut keep_one = TestContextBuilder::new()
            .crates(ctx.config.crates.clone())
            .signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .binary_signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .publisher_allowlist(vec!["npm".to_string(), "github-release".to_string()])
            .build();
        keep_one.config.npms = ctx.config.npms.clone();
        let reqs = collect_requirements(&keep_one, PreflightScope::PublishOnly);
        assert!(
            reqs.iter().any(|r| r.source == "stage:release"),
            "selecting github-release must keep the release requirement: {reqs:?}"
        );
        let cosign_sign_reqs = reqs
            .iter()
            .filter(|r| {
                r.source == "stage:sign"
                    && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
            })
            .count();
        assert_eq!(
            cosign_sign_reqs, 1,
            "selecting a signs consumer must revive the signs: slice; binary_signs: \
             stays publish-only-skipped, so exactly one cosign demand: {reqs:?}"
        );
    }

    /// FULL scope (the main release job's shape) keeps BOTH sign slices: the
    /// `binary_signs:` publish-only gate must NOT fire here, so the binaries
    /// that ship are still signed. Pins the main-job binary-signing invariant
    /// alongside the publish-only deselection above.
    #[test]
    fn full_scope_keeps_both_sign_slices() {
        let top = crate_from_yaml(
            r#"
name: top
release: { github: { owner: o, name: r } }
publish:
  cargo: {}
"#,
        );
        let ctx = TestContextBuilder::new()
            .crates(vec![top])
            .signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .binary_signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .build();
        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        let cosign_sign_reqs = reqs
            .iter()
            .filter(|r| {
                r.source == "stage:sign"
                    && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
            })
            .count();
        assert_eq!(
            cosign_sign_reqs, 2,
            "full scope must keep BOTH sign slices (signs: + binary_signs:): {reqs:?}"
        );
    }

    /// The main release job runs with an EMPTY allowlist + `--skip=npm`, so
    /// neither the release stage nor the `signs:` slice may be deselected:
    /// `publisher_deselected` short-circuits to the denylist alone, which names
    /// only `npm`. This pins the non-regression of the main job alongside the
    /// npm-job deselection above.
    #[test]
    fn empty_allowlist_keeps_release_and_signs_for_main_job() {
        let top = crate_from_yaml(
            r#"
name: top
release: { github: { owner: o, name: r } }
publish:
  cargo: {}
"#,
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![top])
            .signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }])
            .skip_stages(vec!["npm".to_string()])
            .build();
        ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
        let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);
        let has = |source: &str| reqs.iter().any(|r| r.source == source);

        assert!(
            has("stage:release"),
            "empty allowlist must keep the release requirement (main job): {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| {
                r.source == "stage:sign"
                    && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
            }),
            "empty allowlist must keep the signs cosign demand (main job): {reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| r.source == "publish:npm"),
            "--skip=npm must still drop npm: {reqs:?}"
        );
    }

    /// The symmetric denylist case: `--skip=npm` drops npm's requirements
    /// while every other configured publisher keeps contributing.
    #[test]
    fn skip_publisher_drops_only_that_publisher() {
        let top = crate_from_yaml(
            r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![top])
            .skip_stages(vec!["npm".to_string()])
            .build();
        ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
        let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);

        assert!(
            !reqs.iter().any(|r| r.source == "publish:npm"),
            "--skip=npm must drop npm requirements: {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| r.source == "publish:scoop"),
            "--skip=npm must keep other publishers' requirements: {reqs:?}"
        );
    }

    /// The announce-only scope collects the announce surface and nothing
    /// else, even when builds and publishers are configured: announcers
    /// are the only side effects `--announce-only` can produce, so their
    /// secrets are the only thing its preflight may demand.
    #[test]
    fn announce_only_scope_collects_announce_requirements_alone() {
        use anodizer_core::config::{AnnounceConfig, StringOrBool, TelegramAnnounce};
        let top = crate_from_yaml(
            r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
        );
        let mut ctx = TestContextBuilder::new().crates(vec![top]).build();
        ctx.config.announce = Some(AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        });

        let reqs = collect_requirements(&ctx, PreflightScope::AnnounceOnly);
        assert!(
            reqs.iter().all(|r| r.source == "stage:announce"),
            "announce-only scope must collect only announce requirements: {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| matches!(
                &r.requirement,
                EnvRequirement::EnvAllOf { vars } if vars.contains(&"TELEGRAM_TOKEN".to_string())
            )),
            "enabled telegram announcer must demand its token: {reqs:?}"
        );
    }

    /// A config exercising every requirement KIND across the full surface:
    /// a github release token ladder (`EnvAnyOf`), an env-borne cosign key
    /// (`KeyEnv`) plus the cosign tool (`Tool`), an nfpm rpm signature with
    /// a literal `key_file` (`KeyFile`) plus the nfpm tool, a blob endpoint
    /// (`Endpoint`) plus its provider tooling, and a docker image
    /// (`DockerDaemon`). Used by the `SecretsOnly` tests below.
    fn mixed_surface_ctx() -> Context {
        let top = crate_from_yaml(
            r#"
name: top
release: { github: { owner: o, name: r } }
dockers_v2:
  - images: [ghcr.io/o/top]
nfpms:
  - formats: [rpm]
    rpm:
      signature:
        key_file: /etc/anodizer/rpm-signing.gpg
blobs:
  - provider: s3
    bucket: releases
    endpoint: "https://minio.example.com"
"#,
        );
        TestContextBuilder::new()
            .crates(vec![top])
            .signs(vec![anodizer_core::config::SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                args: Some(vec!["--key".to_string(), "env://COSIGN_KEY".to_string()]),
                ..Default::default()
            }])
            .build()
    }

    /// `SecretsOnly` collects the FULL surface (so it sees every credential a
    /// real release would need) then retains ONLY the runner-agnostic
    /// credential kinds — `EnvAllOf` / `EnvAnyOf` / `KeyEnv`. Host-local
    /// kinds (`Tool` / `ToolAnyOf` / `DockerDaemon` / `Endpoint` / `KeyFile`)
    /// are dropped: a decoupled gate runner may not carry the tools, daemon,
    /// network reachability, or on-disk key file that the eventual publish
    /// host does.
    #[test]
    fn secrets_only_retains_credentials_drops_host_local_requirements() {
        let ctx = mixed_surface_ctx();

        // Sanity: the FULL scope over the same config DOES surface the
        // host-local kinds, so the SecretsOnly drop below is meaningful.
        let full = collect_requirements(&ctx, PreflightScope::Full);
        assert!(
            full.iter().any(|r| matches!(
                &r.requirement,
                EnvRequirement::Tool { name } if name == "cosign"
            )),
            "full scope must surface the cosign tool requirement: {full:?}"
        );
        assert!(
            full.iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::DockerDaemon)),
            "full scope must surface the docker daemon requirement: {full:?}"
        );
        assert!(
            full.iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::Endpoint { .. })),
            "full scope must surface the blob endpoint requirement: {full:?}"
        );
        assert!(
            full.iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::KeyFile { .. })),
            "full scope must surface the nfpm key_file requirement: {full:?}"
        );

        let secrets = collect_requirements(&ctx, PreflightScope::SecretsOnly);
        assert!(
            !secrets.is_empty(),
            "secrets-only scope must retain the credential requirements: {secrets:?}"
        );
        // Every retained requirement is a runner-agnostic credential kind.
        for sr in &secrets {
            assert!(
                matches!(
                    &sr.requirement,
                    EnvRequirement::EnvAllOf { .. }
                        | EnvRequirement::EnvAnyOf { .. }
                        | EnvRequirement::KeyEnv { .. }
                ),
                "secrets-only retained a non-credential requirement: {:?}",
                sr
            );
        }
        // The github token ladder (EnvAnyOf) and the env-borne cosign key
        // (KeyEnv) survive — these ARE injected into every decoupled job.
        assert!(
            secrets.iter().any(|r| matches!(
                &r.requirement,
                EnvRequirement::EnvAnyOf { vars }
                    if vars.contains(&"GITHUB_TOKEN".to_string())
            )),
            "secrets-only must retain the github token ladder: {secrets:?}"
        );
        assert!(
            secrets.iter().any(|r| matches!(
                &r.requirement,
                EnvRequirement::KeyEnv { var, .. } if var == "COSIGN_KEY"
            )),
            "secrets-only must retain the env-borne cosign key: {secrets:?}"
        );
        // The host-local kinds are gone.
        assert!(
            !secrets
                .iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::Tool { .. })),
            "secrets-only must drop Tool requirements: {secrets:?}"
        );
        assert!(
            !secrets
                .iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::DockerDaemon)),
            "secrets-only must drop the docker daemon requirement: {secrets:?}"
        );
        assert!(
            !secrets
                .iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::Endpoint { .. })),
            "secrets-only must drop endpoint requirements: {secrets:?}"
        );
        assert!(
            !secrets
                .iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::KeyFile { .. })),
            "secrets-only must drop on-disk key-file requirements: {secrets:?}"
        );
    }

    /// Per-crate workspace mode: a publisher configured ONLY on a workspace
    /// crate still contributes its credential requirement under `SecretsOnly`
    /// (the self-gating publisher derivations walk the full crate universe),
    /// and the host-local AUR tooling is dropped while the env-borne private
    /// key (`KeyEnv` via `env://`) is retained.
    #[test]
    fn secrets_only_unions_workspace_crates() {
        let top = crate_from_yaml(
            r#"
name: top
release: { github: { owner: o, name: r } }
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
            crates: vec![ws_crate],
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![top]).build();
        ctx.config.workspaces = Some(vec![ws]);

        let secrets = collect_requirements(&ctx, PreflightScope::SecretsOnly);
        // The AUR private key reference survives as a retained credential
        // requirement — an env-borne SSH key, which the AUR publisher declares
        // as a structurally-validated `KeyEnv` (not a bare presence check).
        assert!(
            secrets.iter().any(|r| {
                r.source == "publish:aur"
                    && matches!(
                        &r.requirement,
                        EnvRequirement::KeyEnv { var, .. } if var == "PF_TEST_AUR_KEY"
                    )
            }),
            "secrets-only must union the workspace crate's AUR key requirement: {secrets:?}"
        );
        // Every retained requirement is still a credential kind even across
        // the workspace union.
        for sr in &secrets {
            assert!(
                matches!(
                    &sr.requirement,
                    EnvRequirement::EnvAllOf { .. }
                        | EnvRequirement::EnvAnyOf { .. }
                        | EnvRequirement::KeyEnv { .. }
                ),
                "secrets-only retained a non-credential requirement under workspace union: {:?}",
                sr
            );
        }
    }

    /// The build block reports the resolved cross toolchain (not just
    /// `cargo`): a crate cross-compiling to a non-host glibc-Linux target under
    /// the default `auto` strategy must surface `cargo-zigbuild` + `zig` so
    /// `anodizer tools` tells the runner what to install. Skipped on non
    /// x86_64-linux-gnu hosts, where Auto routing differs.
    #[test]
    fn build_block_reports_cross_toolchain_for_cross_target() {
        if anodizer_core::partial::detect_host_target()
            .as_deref()
            .unwrap_or_default()
            != "x86_64-unknown-linux-gnu"
        {
            return;
        }
        let krate = crate_from_yaml(
            r#"
name: app
builds:
  - binary: app
    targets: [aarch64-unknown-linux-gnu]
"#,
        );
        let ctx = TestContextBuilder::new().crates(vec![krate]).build();
        let full = collect_requirements(&ctx, PreflightScope::Full);
        let build_tools: Vec<&str> = full
            .iter()
            .filter(|r| r.source == "stage:build")
            .filter_map(|r| match &r.requirement {
                EnvRequirement::Tool { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        // cargo stays first; the cross toolchain is appended.
        assert_eq!(build_tools.first(), Some(&"cargo"));
        assert!(
            build_tools.contains(&"cargo-zigbuild") && build_tools.contains(&"zig"),
            "build block must report cargo-zigbuild + zig for a cross target: {build_tools:?}"
        );

        // cargo is HARD; the cross toolchain is ADVISORY (the build degrades
        // gracefully without it). A missing zig must warn, never block a
        // release — the regression that aborted preflight on any box without it.
        let advisory_of = |name: &str| -> bool {
            full.iter()
                .find(|r| {
                    r.source == "stage:build"
                        && matches!(&r.requirement, EnvRequirement::Tool { name: n } if n == name)
                })
                .map(|r| r.advisory)
                .unwrap_or_else(|| panic!("missing build tool {name}"))
        };
        assert!(!advisory_of("cargo"), "cargo must stay hard-required");
        assert!(advisory_of("zig"), "zig must be advisory");
        assert!(
            advisory_of("cargo-zigbuild"),
            "cargo-zigbuild must be advisory"
        );

        // Gate behaviour: cargo present, cross toolchain absent ⇒ the report is
        // OK (no failures) with the cross tools demoted to warnings.
        let report = anodizer_core::env_preflight::evaluate(
            &full,
            &|_| None,
            &anodizer_core::env_preflight::EnvProbes {
                tool: &|name| name == "cargo",
                endpoint: &|_| Ok(()),
                docker: &|| true,
            },
        );
        assert!(
            report.ok(),
            "release must not be blocked by a missing cross toolchain: {report}"
        );
        assert!(
            report.warnings.iter().any(|w| w.message.contains("zig")),
            "missing zig must surface as a warning: {:?}",
            report.warnings
        );

        // SecretsOnly still drops every Tool requirement — the cross toolchain
        // must NOT leak into the pre-tag secrets gate.
        let secrets = collect_requirements(&ctx, PreflightScope::SecretsOnly);
        assert!(
            !secrets
                .iter()
                .any(|r| matches!(&r.requirement, EnvRequirement::Tool { .. })),
            "secrets-only must drop the cross toolchain Tool reqs: {secrets:?}"
        );
    }

    /// A `signs:` cosign config keyed off `env://COSIGN_KEY` must surface as a
    /// distinct `env://COSIGN_KEY` ref for the offline load verification, and
    /// duplicate refs (cosign declared on two crates) collapse to one.
    #[test]
    fn cosign_key_refs_reconstructs_env_scheme_and_dedups() {
        let reqs = vec![
            SourcedRequirement::new(
                "stage:sign",
                EnvRequirement::KeyEnv {
                    kind: anodizer_core::KeyKind::Cosign,
                    var: "COSIGN_KEY".to_string(),
                },
            ),
            SourcedRequirement::new(
                "stage:sign",
                EnvRequirement::KeyEnv {
                    kind: anodizer_core::KeyKind::Cosign,
                    var: "COSIGN_KEY".to_string(),
                },
            ),
            // A non-cosign key kind must NOT be picked up by the cosign verify.
            SourcedRequirement::new(
                "publish:aur",
                EnvRequirement::KeyEnv {
                    kind: anodizer_core::KeyKind::SshPrivate,
                    var: "AUR_SSH_KEY".to_string(),
                },
            ),
        ];
        assert_eq!(cosign_key_refs(&reqs), vec!["env://COSIGN_KEY".to_string()]);
    }

    /// No cosign key in the requirement set ⇒ nothing to verify ⇒ no WARN, no
    /// error, returns `true` (the gate stays clean for configs without cosign).
    #[test]
    fn verify_cosign_keys_load_noop_without_cosign_config() {
        let reqs = vec![SourcedRequirement::new(
            "stage:build",
            EnvRequirement::Tool {
                name: "cargo".to_string(),
            },
        )];
        let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
        assert!(verify_cosign_keys_load(&reqs, &log));
        assert!(
            capture.all_messages().is_empty(),
            "no cosign requirement must emit nothing: {:?}",
            capture.all_messages()
        );
    }

    /// When cosign is NOT on PATH, an active cosign-key config must WARN (load
    /// verification deferred to sign time) and NOT hard-fail. The absent outcome
    /// is injected via the load-resolver seam so the WARN branch runs on every
    /// shard — including CI shards that DO carry cosign — rather than self-skipping.
    #[test]
    fn verify_cosign_keys_load_warns_when_cosign_absent() {
        use anodizer_core::log::LogLevel;
        let reqs = vec![SourcedRequirement::new(
            "stage:sign",
            EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::Cosign,
                var: "COSIGN_KEY".to_string(),
            },
        )];
        let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
        // Force the cosign-absent outcome deterministically, independent of PATH.
        let all_loaded = verify_cosign_keys_load_with(&reqs, &log, |_| {
            anodizer_stage_sign::CosignKeyLoad::CosignUnavailable
        });
        // cosign-absent is NOT a failure.
        assert!(all_loaded, "cosign-absent must not fail the gate");
        let msgs = capture.all_messages();
        assert!(
            msgs.iter()
                .any(|(lvl, m)| *lvl == LogLevel::Warn && m.contains("cosign not installed")),
            "cosign-absent must WARN: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|(lvl, _)| *lvl == LogLevel::Error),
            "cosign-absent must NOT emit an error: {msgs:?}"
        );
    }

    /// A genuinely bad key (cosign installed, load fails) must FAIL the gate
    /// and emit an Error. Injected via the seam so it runs deterministically
    /// regardless of PATH.
    #[test]
    fn verify_cosign_keys_load_fails_on_bad_key() {
        use anodizer_core::log::LogLevel;
        let reqs = vec![SourcedRequirement::new(
            "stage:sign",
            EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::Cosign,
                var: "COSIGN_KEY".to_string(),
            },
        )];
        let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
        let all_loaded = verify_cosign_keys_load_with(&reqs, &log, |_| {
            anodizer_stage_sign::CosignKeyLoad::Failed("wrong COSIGN_PASSWORD".to_string())
        });
        assert!(!all_loaded, "a failing key load must fail the gate");
        let msgs = capture.all_messages();
        assert!(
            msgs.iter()
                .any(|(lvl, m)| *lvl == LogLevel::Error && m.contains("failed to load")),
            "a bad key must emit an Error: {msgs:?}"
        );
    }
}
