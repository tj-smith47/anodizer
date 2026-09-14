//! `anodizer preflight` — the one preflight engine.
//!
//! [`run_engine`] answers three questions in one pass, with zero mutations:
//! * environment — every enabled stage and publisher declares what it needs
//!   from the runner (CLI tools, env vars/secrets by presence only, endpoint
//!   reachability, docker daemon, loadable key material), evaluated in one
//!   collect-all pass so the operator sees the complete fix list at once;
//! * publishers — each one-way-door publisher is asked whether the target
//!   version is already submitted, pending or published, and each
//!   publisher's credential is probed;
//! * reconcile — each selected publisher reports whether the target version
//!   is already upstream with these bytes.
//!
//! Two consumers share it, and nothing else runs any half of it:
//! * the standalone `anodizer preflight` command (CI canary / local check),
//!   which derives the target version the way `anodizer tag` would;
//! * `anodizer release`, which runs it once before any stage and aborts
//!   before side effects when anything is wrong (`--skip=preflight` opts out).
//!
//! Requirements are declared next to the code that consumes them — each
//! stage crate exports `env_requirements(ctx)` and each publisher
//! implements `Publisher::requirements` — so the preflight surface cannot
//! drift from what the stages actually read.

use std::path::PathBuf;
use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::env_preflight::{self, EnvPreflightReport, EnvProbes, SourcedRequirement};
use anodizer_core::git::{TagPosition, TagSource};
use anodizer_core::log::StageLogger;
use anodizer_stage_publish::reconcile_report::ReconcileReport;
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
    /// `anodizer release --publish-only`: exactly the stage set of
    /// `build_publish_only_pipeline`. Membership is DERIVED from that
    /// builder (see [`PreflightScope::included_stage_set`]) so a stage
    /// added to the publish-only pipeline is preflighted here
    /// automatically instead of reporting success and aborting at runtime.
    PublishOnly,
    /// `anodizer release --announce-only`: re-fires announcers against a
    /// prior run's report — announce is the only stage that runs, so only
    /// its requirements apply.
    AnnounceOnly,
}

impl PreflightScope {
    /// The stage-name membership for scopes that guard a REDUCED pipeline,
    /// derived from the very builder that assembles the pipeline the scope
    /// guards — never a hand-maintained mirror list (the mirror drifted:
    /// stages added to the publish-only pipeline were absent from it).
    /// `None` means every stage participates.
    fn included_stage_set(self) -> Option<std::collections::BTreeSet<String>> {
        let pipeline = match self {
            PreflightScope::Full => return None,
            PreflightScope::PublishOnly => crate::pipeline::build_publish_only_pipeline(),
            PreflightScope::AnnounceOnly => crate::pipeline::build_announce_pipeline(),
        };
        Some(
            pipeline
                .stage_names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }
}

/// Which runtime skip predicate gates a stage's requirement collection.
enum StageGate {
    /// The `--skip` stage denylist (`ctx.should_skip`).
    Skip,
    /// Stages that self-skip at runtime when a `--publishers` allowlist (or
    /// `--skip`) deselects them (docker/docker-sign/blob/snapcraft-publish/
    /// announce) must gate their preflight requirements on the SAME predicate
    /// the runtime uses (`ctx.publisher_deselected`), or an npm-only
    /// `--publishers npm` run is falsely blocked by cosign/minio/snapcraft
    /// tooling those stages will never reach. Mirrors the publish-loop's
    /// `publisher_deselected` lockstep.
    Selected,
}

/// A [`GatedStage`] requirement builder: the `(source, requirements)`
/// pairs a stage contributes when its gate admits it.
type SourcedStageRequirements = Vec<(String, Vec<anodizer_core::EnvRequirement>)>;

/// One requirement-gated stage: its release-pipeline stage name, the runtime
/// skip predicate its gate must mirror, and the builder producing its
/// `(source, requirements)` pairs.
struct GatedStage {
    stage: &'static str,
    gate: StageGate,
    requirements: fn(&Context, PreflightScope) -> SourcedStageRequirements,
}

/// The common [`GATED_STAGES`] row shape: one `stage:<name>` source fed by
/// the stage crate's requirement derivation.
macro_rules! gated_stage {
    ($name:literal, $gate:ident, $reqs:path) => {
        GatedStage {
            stage: $name,
            gate: StageGate::$gate,
            requirements: |ctx, _scope| vec![(concat!("stage:", $name).to_string(), $reqs(ctx))],
        }
    };
}

/// The SINGLE enumeration of every stage whose requirements preflight
/// collects. [`collect_requirements`] iterates this table and the
/// stage-name existence test pins every entry against the real release
/// pipeline, so a new gated stage cannot be added to the collection loop
/// without also being covered by the test (they are the same list).
const GATED_STAGES: &[GatedStage] = &[
    // Build stage: the run path spawns the literal `cargo` from PATH, so
    // probe exactly that, then add the cross-compilation toolchain the build
    // resolves per target (cargo-zigbuild + zig, cross, or a system cross gcc)
    // so `anodizer tools` reports what a runner must install to cross-compile
    // instead of the action re-deriving it in bash.
    //
    // `cargo` is HARD-required here: the build literally spawns it from
    // PATH. The cross-compilation toolchain is ADVISORY and appended after
    // the table loop in `collect_requirements` (the build degrades
    // gracefully without it, so a missing zig/cargo-zigbuild must warn, not
    // block the gate); the advisory half reuses this row's gate decision so
    // the two halves can never gate on divergent predicates.
    GatedStage {
        stage: "build",
        gate: StageGate::Skip,
        requirements: |_ctx, _scope| {
            vec![(
                "stage:build".to_string(),
                vec![anodizer_core::EnvRequirement::Tool {
                    name: "cargo".to_string(),
                }],
            )]
        },
    },
    gated_stage!("nfpm", Skip, anodizer_stage_nfpm::env_requirements),
    gated_stage!("srpm", Skip, anodizer_stage_srpm::env_requirements),
    gated_stage!(
        "snapcraft",
        Skip,
        anodizer_stage_snapcraft::build_env_requirements
    ),
    gated_stage!(
        "snapcraft-publish",
        Selected,
        anodizer_stage_snapcraft::publish_env_requirements
    ),
    // The release pipeline's sign stage drives both `signs:` and
    // `binary_signs:` (BinarySignStage is the `anodizer build` selection), so
    // both slices hang off the `sign` skip gate here. Both produce detached
    // signature assets that only the publishers in `signs_consumers()` read,
    // so one gate mirrors what `SignStage::run` does at runtime: when EVERY
    // one of those publishers is deselected the stage skips both loops, and
    // preflight must not demand cosign/GPG material for signatures the run
    // will never produce (the npm-only `--publishers npm` job). An empty
    // allowlist deselects nothing, so the main release job still demands both.
    GatedStage {
        stage: "sign",
        gate: StageGate::Skip,
        requirements: |ctx, _scope| {
            if anodizer_stage_sign::signs_fully_deselected(ctx) {
                return Vec::new();
            }
            vec![
                (
                    "stage:sign".to_string(),
                    anodizer_stage_sign::sign_env_requirements(ctx),
                ),
                (
                    "stage:sign".to_string(),
                    anodizer_stage_sign::binary_sign_env_requirements(ctx),
                ),
            ]
        },
    },
    gated_stage!(
        "docker-sign",
        Selected,
        anodizer_stage_sign::docker_sign_env_requirements
    ),
    gated_stage!("sbom", Skip, anodizer_stage_sbom::env_requirements),
    gated_stage!("makeself", Skip, anodizer_stage_makeself::env_requirements),
    gated_stage!(
        "install-script",
        Skip,
        anodizer_stage_install_script::env_requirements
    ),
    gated_stage!("upx", Skip, anodizer_stage_upx::env_requirements),
    gated_stage!("appimage", Skip, anodizer_stage_appimage::env_requirements),
    gated_stage!("docker", Selected, anodizer_stage_docker::env_requirements),
    gated_stage!("blob", Selected, anodizer_stage_blob::env_requirements),
    gated_stage!(
        "verify-release",
        Skip,
        anodizer_stage_verify_release::env_requirements
    ),
    // Per-platform bundler stages: each gates itself on the configured
    // build targets (a darwin-only matrix never demands makensis, a
    // --single-target host release never demands cross-platform tools).
    gated_stage!("msi", Skip, anodizer_stage_msi::env_requirements),
    gated_stage!("nsis", Skip, anodizer_stage_nsis::env_requirements),
    gated_stage!("pkg", Skip, anodizer_stage_pkg::env_requirements),
    gated_stage!("dmg", Skip, anodizer_stage_dmg::env_requirements),
    gated_stage!(
        "appbundle",
        Skip,
        anodizer_stage_appbundle::env_requirements
    ),
    gated_stage!("flatpak", Skip, anodizer_stage_flatpak::env_requirements),
    gated_stage!("notarize", Skip, anodizer_stage_notarize::env_requirements),
    gated_stage!(
        "announce",
        Selected,
        anodizer_stage_announce::env_requirements
    ),
    // Release stage: GitHub release creation + asset upload authenticate
    // via the github-release publisher's ladder. The publisher is also in
    // the registry (the `publish` row below), but the release stage runs even
    // when `publish` is skipped — declare it under its own gate (the
    // evaluator dedups).
    //
    // github-release is a real publisher, so the release stage self-skips at
    // runtime when a `--publishers` allowlist deselects it (keyed on the
    // PUBLISHER name `github-release`, the same predicate `ReleaseStage::run`
    // and the publish-loop use) — gate the preflight demand on that too,
    // so an npm-only `--publishers npm` run is not asked for the GitHub token
    // ladder for a release it will never create. The `--skip` stage gate stays
    // on the STAGE name `release` (this row's Skip gate), since
    // `--skip=release` is a stage-name denylist; the publisher-name gate is
    // additive on top of it. An EMPTY allowlist deselects nothing, so the
    // main release job (empty allowlist + `--skip=npm`) still demands the
    // ladder.
    GatedStage {
        stage: "release",
        gate: StageGate::Skip,
        requirements: |ctx, _scope| {
            let release_skipped = ctx
                .config
                .release
                .as_ref()
                .and_then(|r| r.skip.as_ref())
                .is_some_and(|s| {
                    s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .unwrap_or(false)
                });
            if ctx.publisher_deselected("github-release") || release_skipped {
                return Vec::new();
            }
            vec![(
                "stage:release".to_string(),
                anodizer_core::Publisher::requirements(
                    &anodizer_stage_release::publisher::GithubReleasePublisher::new(),
                    ctx,
                ),
            )]
        },
    },
    // The full registry, not `configured_publishers`: requirement derivation
    // must see publishers configured only on workspace crates, which the
    // registry's top-level-crate predicates cannot — each
    // `Publisher::requirements` self-gates on the resolved config instead.
    // Publishers DESELECTED at runtime via `--publishers` (allowlist) or
    // `--skip` (denylist) are excluded so their secrets/tools cannot gate a
    // run that will never invoke them — `publisher_deselected` is the same
    // predicate the publish dispatch layer uses, keeping preflight and
    // dispatch in lockstep across every config mode.
    GatedStage {
        stage: "publish",
        gate: StageGate::Skip,
        requirements: |ctx, _scope| {
            anodizer_stage_publish::registry::all_publishers()
                .into_iter()
                .filter(|publisher| !ctx.publisher_deselected(publisher.name()))
                .map(|publisher| {
                    (
                        format!("publish:{}", publisher.name()),
                        publisher.requirements(ctx),
                    )
                })
                .collect()
        },
    },
];

/// Collect every environment requirement the resolved config implies,
/// honoring `--skip` stage selection and the pipeline scope, by walking
/// the [`GATED_STAGES`] table. Per-crate workspace configs union across
/// all publishable crates: the stage/publisher derivations walk the full
/// crate universe, so one pass covers single-crate, lockstep, and
/// per-crate modes alike.
pub fn collect_requirements(ctx: &Context, scope: PreflightScope) -> Vec<SourcedRequirement> {
    let mut out: Vec<SourcedRequirement> = Vec::new();
    // Computed once per collection pass: the reduced-scope stage membership
    // derives from the pipeline builders (see `included_stage_set`).
    let included = scope.included_stage_set();
    let in_scope =
        |stage: &str| -> bool { included.as_ref().is_none_or(|set| set.contains(stage)) };

    // The build stage's preflight is split across two sites — the HARD
    // `cargo` probe in its table row and the ADVISORY cross-toolchain append
    // after the loop. The gate is decided ONCE, at the table row, so the two
    // halves can never gate on divergent predicates.
    let mut build_runs = false;
    let mut publish_runs = false;
    let mut nfpm_runs = false;
    for gated in GATED_STAGES {
        let stage_on = in_scope(gated.stage)
            && match gated.gate {
                StageGate::Skip => !ctx.should_skip(gated.stage),
                StageGate::Selected => !ctx.publisher_deselected(gated.stage),
            };
        match gated.stage {
            "build" => build_runs = stage_on,
            "publish" => publish_runs = stage_on,
            "nfpm" => nfpm_runs = stage_on,
            _ => {}
        }
        if !stage_on {
            continue;
        }
        for (source, reqs) in (gated.requirements)(ctx, scope) {
            out.extend(
                reqs.into_iter()
                    .map(|r| SourcedRequirement::new(&source, r)),
            );
        }
    }

    // Cross-compilation toolchain — ADVISORY. The build stage resolves a
    // strategy per target and degrades gracefully when the preferred tool is
    // absent (zigbuild → cargo → system gcc; see stage-build's
    // `detect_cross_strategy`), so a missing `zig`/`cargo-zigbuild` must WARN,
    // never block a release. `anodizer tools` still self-reports these as the
    // recommended toolchain.
    if build_runs {
        out.extend(
            anodizer_stage_build::cross_tool_requirements(ctx)
                .into_iter()
                .map(|r| SourcedRequirement::new_advisory("stage:build", r)),
        );
    }

    // Publisher ADVISORY requirements — optional validators (`ruby -c`,
    // `bash -n`, `nix-instantiate --parse`) and preferred transports (`gh`)
    // whose absence degrades a publish gracefully instead of failing it.
    // Same gate + deselection predicate as the hard `publish` table row, so
    // a deselected publisher can never surface even a warn for tools it will
    // not use.
    if publish_runs {
        for publisher in anodizer_stage_publish::registry::all_publishers() {
            if ctx.publisher_deselected(publisher.name()) {
                continue;
            }
            let source = format!("publish:{}", publisher.name());
            out.extend(
                publisher
                    .advisory_requirements(ctx)
                    .into_iter()
                    .map(|r| SourcedRequirement::new_advisory(&source, r)),
            );
        }
    }

    // nfpm's schema floor cross-checks built .deb/.rpm packages with the
    // native tooling (`dpkg-deb --info`, `rpm -qp`) when present, and
    // warn+skips when absent — advisory, reusing the nfpm row's gate
    // decision like the build stage's cross-toolchain append.
    if nfpm_runs {
        out.extend(
            anodizer_stage_nfpm::advisory_env_requirements(ctx)
                .into_iter()
                .map(|r| SourcedRequirement::new_advisory("stage:nfpm", r)),
        );
    }

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

/// Distinct gpg signing commands the collected requirements declare: every
/// `Tool` requirement sourced from the sign/docker-sign stages whose command
/// classifies as gpg (`anodizer_core::signing::is_gpg_command`). Deduped so
/// several sign configs sharing one cmd probe it once. Requirements are
/// collected AFTER stage/publisher gating, so a skipped or deselected sign
/// surface never contributes a probe.
fn gpg_sign_cmds(requirements: &[SourcedRequirement]) -> Vec<String> {
    let mut cmds: Vec<String> = Vec::new();
    for sr in requirements {
        if sr.source != "stage:sign" && sr.source != "stage:docker-sign" {
            continue;
        }
        if let anodizer_core::EnvRequirement::Tool { name } = &sr.requirement
            && anodizer_core::signing::is_gpg_command(name)
            && !cmds.contains(name)
        {
            cmds.push(name.clone());
        }
    }
    cmds
}

/// Probe every configured gpg signing command for `--faked-system-time`
/// support. When `SOURCE_DATE_EPOCH` is set, the sign stage injects
/// `--faked-system-time=<epoch>!` into every gpg invocation to pin the
/// OpenPGP signature timestamp — a gpg too old for the flag (< 2.0.10)
/// would otherwise die mid-pipeline with a raw "invalid option" after the
/// full build.
///
/// Returns `true` when every gpg cmd accepts the flag (or none is
/// configured). A failing probe is a hard failure only when
/// `SOURCE_DATE_EPOCH` is currently set (the injection WILL fire this
/// run); otherwise it WARNs about the latent incompatibility.
fn verify_gpg_faked_system_time(
    requirements: &[SourcedRequirement],
    ctx: &Context,
    log: &StageLogger,
) -> bool {
    let sde_set = ctx.env_var("SOURCE_DATE_EPOCH").is_some();
    verify_gpg_faked_system_time_with(requirements, sde_set, log, |cmd| {
        // A gpg missing from PATH entirely is already reported by the Tool
        // requirement's own evaluation — don't double-report it as a
        // flag-support failure.
        if !anodizer_core::tool_detect::on_path(cmd) {
            return true;
        }
        anodizer_core::tool_detect::tool_runs_with_args(
            cmd,
            &["--faked-system-time=19700101T000000!", "--version"],
        )
    })
}

/// Inner of [`verify_gpg_faked_system_time`] with the probe and the
/// `SOURCE_DATE_EPOCH` presence injected, so tests can drive both branches
/// deterministically regardless of the host's gpg and process env.
fn verify_gpg_faked_system_time_with(
    requirements: &[SourcedRequirement],
    sde_set: bool,
    log: &StageLogger,
    probe: impl Fn(&str) -> bool,
) -> bool {
    let mut all_support = true;
    for cmd in gpg_sign_cmds(requirements) {
        if probe(&cmd) {
            log.status(&format!(
                "{cmd} accepts --faked-system-time (deterministic signature timestamps)"
            ));
            continue;
        }
        if sde_set {
            all_support = false;
            log.error(&format!(
                "{cmd} rejects --faked-system-time (gpg < 2.0.10?) — SOURCE_DATE_EPOCH is set, \
                 so the sign stage will inject the flag and gpg will fail mid-release"
            ));
        } else {
            log.warn(&format!(
                "{cmd} rejects --faked-system-time (gpg < 2.0.10?) — harmless now, but any run \
                 with SOURCE_DATE_EPOCH set injects the flag and fails at sign time"
            ));
        }
    }
    all_support
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
    if !verify_gpg_faked_system_time(&requirements, ctx, log) {
        report.note_failure(
            "stage:sign",
            "gpg rejects --faked-system-time while SOURCE_DATE_EPOCH is set",
        );
    }
    report
}

/// Everything one preflight run found, across its three halves.
pub(crate) struct PreflightOutcome {
    /// Tools, secrets, endpoints and key material the enabled stages need.
    pub environment: EnvPreflightReport,
    /// The one-way-door publisher states and credential probes; `None` when
    /// the run publishes nothing (`--skip=publish`, `--announce-only`).
    pub publishers: Option<anodizer_core::preflight::PreflightReport>,
    /// Whether the target version is already upstream, per selected publisher.
    pub reconcile: ReconcileReport,
}

impl PreflightOutcome {
    /// Whether every half passed: no environment failure, no publisher
    /// blocker, and no REQUIRED publisher diverged.
    pub(crate) fn ok(&self) -> bool {
        self.environment.ok()
            && self
                .publishers
                .as_ref()
                .is_none_or(|report| report.blockers.is_empty())
            && self.reconcile.blocking().is_empty()
    }

    /// The message a failed run aborts with, naming the first failing half in
    /// the order an operator fixes them: a missing credential is a runner
    /// problem, a blocker is a config problem, a divergence is a version
    /// problem.
    pub(crate) fn failure_message(&self) -> String {
        if !self.environment.ok() {
            return preflight_failure_message(&self.environment);
        }
        if let Some(report) = self.publishers.as_ref()
            && !report.blockers.is_empty()
        {
            return format!(
                "preflight: {} resilience blocker(s): {}",
                report.blockers.len(),
                report.blockers.join("; "),
            );
        }
        let blocking = self.reconcile.blocking().len();
        format!(
            "preflight: {blocking} required publisher(s) diverged — the version is already \
             published with different content; bump the version"
        )
    }
}

/// The message a failed environment preflight aborts with: how many checks
/// failed, out of how many that ran, and where to look.
pub(crate) fn preflight_failure_message(report: &EnvPreflightReport) -> String {
    format!(
        "preflight: {} environment failure(s) across {} check(s); \
         fix the issues above before re-running",
        report.failures.len(),
        report.checks
    )
}

/// Run the whole preflight once: the environment half, then the publisher
/// half, then the reconcile sweep. Every half is reported as it completes so
/// a failing environment still shows the publisher table beneath it. The
/// caller decides whether a failed outcome aborts (release) or exits
/// non-zero (standalone).
///
/// The publisher half is skipped when `publish` is skipped or the scope is
/// [`PreflightScope::AnnounceOnly`]: neither run crosses a one-way door.
pub(crate) fn run_engine(
    ctx: &mut Context,
    scope: PreflightScope,
    log: &StageLogger,
) -> Result<PreflightOutcome> {
    let environment = run_env_preflight(ctx, scope, log);

    let publishers = if scope == PreflightScope::AnnounceOnly {
        log.status("skipped publisher preflight — --announce-only does not publish");
        None
    } else if ctx.should_skip("publish") {
        log.verbose("skipped publisher preflight — publish is skipped");
        None
    } else {
        let report = anodizer_stage_publish::preflight::run_preflight(ctx, log)?;
        if report.entries.is_empty() {
            log.verbose("skipped one-way-door preflight — no one-way-door publishers configured");
        } else {
            report.emit(log);
        }
        if report.blockers.is_empty() {
            log.status(&format!(
                "preflight found {} publisher(s) clean",
                report.clean_count()
            ));
        }
        Some(report)
    };

    // Publisher state is a second, independent axis: the environment report
    // answers "can this runner publish?", the reconcile table answers "is it
    // already published?". Probing each SELECTED publisher through the same
    // `reconcile()` the dispatch loop calls is what keeps the canary and the
    // release from drifting into two answers.
    let reconcile = match reconcile_sweep(ctx, log) {
        ReconcileSweep::Stale { reason } => ReconcileReport::skipped(reason),
        ReconcileSweep::Applies => ReconcileReport::probe(ctx),
    };
    reconcile.emit(log);

    Ok(PreflightOutcome {
        environment,
        publishers,
        reconcile,
    })
}

/// Whether the reconcile sweep is a question about the version this tree
/// would release.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileSweep {
    /// The resolved version is the one this run would publish, so every
    /// publisher's verdict is actionable — including a required `Diverged`,
    /// which must still gate.
    Applies,
    /// The resolved version is already released and HEAD has moved past it.
    Stale { reason: String },
}

/// Decide whether the reconcile sweep applies to the tree being inspected.
///
/// The table answers "is THIS version already upstream with THESE bytes?",
/// which is only meaningful while the resolved version is the version this run
/// would publish. Between two releases it is not: the context resolves the
/// latest existing tag, so on any tree with commits after its last release
/// every probe reports on the PREVIOUS version — the `Diverged` rows are true
/// and irrelevant (the tree moved on, a higher version will be cut) and the
/// `absent — will publish` rows describe a version nobody will publish.
/// Skipping the whole sweep trades a purely local git query for one network
/// probe per publisher, and is what keeps a required `Diverged` on the last
/// release from hard-failing the run that cuts the next one.
///
/// The tag name comes from `git_info.tag` — the exact ref the context resolved
/// (latest matching tag, `ANODIZER_CURRENT_TAG` override, or the synthetic
/// `v0.0.0` for a tagless repo) and the very string the `Tag` template var is
/// derived from. It is correct across config modes for the same reason it is
/// what git resolved in the first place: lockstep configs resolve one shared
/// `v{{ Version }}` ref, per-crate configs resolve the first selected crate's
/// `{name}-v{{ Version }}` ref, and monorepo mode strips its prefix only on
/// the way into the `Tag` VAR, never in `git_info.tag`.
///
/// The staleness inference is only sound for an INFERRED tag. A
/// [`TagSource::Declared`] tag is the operator naming the version this run
/// targets — a backfill canary run from a tree checked out ahead of the tag it
/// is publishing is exactly that shape — so the position of that tag relative
/// to `HEAD` says nothing about whether it is the version being published, and
/// the sweep always applies.
///
/// A git failure yields [`ReconcileSweep::Applies`]: an unanswerable position
/// question must not silently disable the divergence gate.
fn reconcile_sweep(ctx: &Context, log: &StageLogger) -> ReconcileSweep {
    let Some(git_info) = ctx.git_info.as_ref() else {
        return ReconcileSweep::Applies;
    };
    if git_info.tag_source == TagSource::Declared {
        return ReconcileSweep::Applies;
    }
    let tag = git_info.tag.as_str();
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    match anodizer_core::git::tag_position_in(&root, tag) {
        // A tag that does not exist is the version about to be cut; a tag AT
        // HEAD is the genuine resume / backfill / `--publish-only` case, where
        // a required `Diverged` is exactly the signal the operator needs.
        Ok(TagPosition::Missing) | Ok(TagPosition::AtHead) => ReconcileSweep::Applies,
        Ok(TagPosition::AncestorOfHead) => ReconcileSweep::Stale {
            reason: format!(
                "{tag} is already released and HEAD has advanced past it; \
                 this tree will cut a new version"
            ),
        },
        // A tag off HEAD's history (an older checkout, a divergent branch) is
        // just as unpublishable from here, but claiming HEAD advanced past it
        // would be false.
        Ok(TagPosition::UnrelatedToHead) => ReconcileSweep::Stale {
            reason: format!(
                "{tag} is already released and HEAD is not on its history; \
                 this tree will not publish that version"
            ),
        },
        Err(e) => {
            log.verbose(&format!(
                "could not locate tag {tag} relative to HEAD, probing anyway: {e:#}"
            ));
            ReconcileSweep::Applies
        }
    }
}

mod standalone;
pub use standalone::{PreflightOpts, run};
#[cfg(test)]
mod tests;
