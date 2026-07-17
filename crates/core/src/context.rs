use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::env_source::{EnvSource, ProcessEnvSource};
use crate::git::GitInfo;
use crate::log::{StageLogger, Verbosity};
use crate::partial::PartialTarget;
use crate::publish_report::PublishReport;
use crate::publisher_kind::PublisherKind;
use crate::scm::ScmTokenType;
use crate::template::TemplateVars;
use crate::verify_release_summary::VerifyReleaseSummary;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use strum::IntoEnumIterator;

/// Rollback policy after the publish stage. `BestEffort` is the default when
/// pre-flight ran clean; `None` is the implicit default otherwise (callers
/// should warn that rollback is disabled). The CLI flag `--rollback=<v>`
/// sets `ContextOptions::rollback_mode` to `Some(v)` to override the
/// default-resolution at the dispatch site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RollbackMode {
    /// Do not attempt rollback. Useful when the operator wants to inspect
    /// half-published state before deciding.
    None,
    /// Run best-effort rollback for every reversible publisher whose
    /// evidence is present in the report. Most irreversible publishers
    /// (chocolatey moderation, winget PRs, AUR) are never rolled back —
    /// the Submitter gate is their only protection. The exception is
    /// cargo: a partial multi-crate publish that left live crates records
    /// them and gets those crates yanked even on a failed run.
    #[default]
    BestEffort,
}

/// Non-publisher `--skip` tokens for the `release` command: the pipeline
/// stage / phase names that are NOT publishers.
///
/// The publisher tokens are NOT listed here — they are derived from
/// [`PublisherKind`] and unioned in by [`VALID_RELEASE_SKIPS`], so the
/// `--skip` publisher vocabulary cannot drift from the registry. Keep ONLY
/// non-publisher stage tokens here.
///
/// Two pairs look like publishers but are stages and belong here:
/// `snapcraft` is the snap *build* stage (its publisher sibling is
/// `snapcraft-publish`), and `release` is the GitHub/GitLab/Gitea release
/// *stage* (its publisher sibling is `github-release`).
const NON_PUBLISHER_RELEASE_SKIPS: &[&str] = &[
    "publish",
    "sign",
    "validate",
    "sbom",
    "attest",
    "snapcraft",
    "nfpm",
    "makeself",
    "install-script",
    "appimage",
    "flatpak",
    "srpm",
    "before",
    "before-publish",
    "notarize",
    "archive",
    "source",
    "build",
    "changelog",
    "release",
    "checksum",
    "upx",
    "templatefiles",
    "dmg",
    "msi",
    "nsis",
    "pkg",
    "appbundle",
    "verify-release",
];

/// Valid `--skip` values for the `release` command: every pipeline
/// stage/phase token ([`NON_PUBLISHER_RELEASE_SKIPS`]) PLUS every publisher
/// token (derived from [`PublisherKind`]).
///
/// Skip tokens are stage names plus publisher names. Every publisher's skip
/// token is its canonical [`crate::Publisher::name`] / [`PublisherKind::token`]
/// (the same token `--publishers` keys on and the same one GoReleaser's
/// `--skip` uses), so homebrew is `homebrew` and chocolatey is `chocolatey` —
/// there are no short aliases (`brew`/`choco`). This keeps one denylist
/// vocabulary across the `--skip` and `--publishers` selectors and matches
/// GoReleaser's `--skip` keys, so a single name works on both tools.
///
/// Deriving the publisher half from [`PublisherKind::iter`] is what makes the
/// vocabulary drift-proof: a newly added publisher is automatically a valid
/// `--skip` token. (This closed a real gap — nine publisher tokens
/// — `npm`, `gemfury`, `cloudsmith`, `artifactory`, `uploads`, `dockerhub`,
/// `mcp`, `schemastore`, `upstream-aur` — had silently fallen out of the old
/// hand-maintained literal.)
pub static VALID_RELEASE_SKIPS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    NON_PUBLISHER_RELEASE_SKIPS
        .iter()
        .copied()
        .chain(PublisherKind::iter().map(PublisherKind::token))
        .collect()
});

/// One entry in anodizer's canonical `--skip` / `--publishers` vocabulary,
/// emitted by `anodizer vocabulary` for machine consumers (the GitHub Action
/// derives its skip / publisher token sets from this instead of re-deriving
/// them in shell).
///
/// `is_publisher` marks the publisher tokens (the half of the vocabulary that
/// `--publishers` also accepts); `is_publish_stage` mirrors
/// [`PublisherKind::is_publish_stage`] for those, and is always `false` for
/// the non-publisher pipeline-stage tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReleaseToken {
    /// The canonical lowercase token, exactly as `--skip` / `--publishers`
    /// key on it (e.g. `homebrew`, never `homebrew-cask`; `uploads`, never
    /// `upload`).
    pub token: &'static str,
    /// `true` for the publisher half of the vocabulary — the tokens
    /// `--publishers` also accepts. `false` for non-publisher stage tokens.
    pub is_publisher: bool,
    /// `true` when this is a publisher that fires its publish from a pipeline
    /// stage rather than the trait-dispatch chokepoint (see
    /// [`PublisherKind::is_publish_stage`]). Always `false` for non-publisher
    /// stage tokens.
    pub is_publish_stage: bool,
}

/// The full canonical `--skip` / `--publishers` vocabulary as structured
/// entries, derived entirely from [`NON_PUBLISHER_RELEASE_SKIPS`] and
/// [`PublisherKind::iter`] — no hand-maintained list. Adding a publisher
/// variant or a non-publisher stage token updates this automatically.
///
/// The set of [`ReleaseToken::token`] values equals [`VALID_RELEASE_SKIPS`]
/// exactly (enforced by a by-construction test), so anodizer and its
/// consumers can never disagree on the legal token set.
pub fn release_skip_vocabulary() -> Vec<ReleaseToken> {
    NON_PUBLISHER_RELEASE_SKIPS
        .iter()
        .map(|&token| ReleaseToken {
            token,
            is_publisher: false,
            is_publish_stage: false,
        })
        .chain(PublisherKind::iter().map(|k| ReleaseToken {
            token: k.token(),
            is_publisher: true,
            is_publish_stage: k.is_publish_stage(),
        }))
        .collect()
}

/// Valid --skip values for the `build` command.
pub const VALID_BUILD_SKIPS: &[&str] = &["pre-hooks", "post-hooks", "validate", "before"];

/// Validate that all skip values are in the allowed set.
///
/// Returns `Ok(())` if all values are valid, or `Err` with a descriptive
/// message listing the invalid value(s) and the full set of valid options.
pub fn validate_skip_values(skip: &[String], valid: &[&str]) -> Result<(), String> {
    let invalid: Vec<&str> = dedup_preserving_order(
        skip.iter()
            .map(|s| s.as_str())
            .filter(|s| !valid.contains(s)),
    );
    if invalid.is_empty() {
        Ok(())
    } else {
        // The combined skip vocabulary is `VALID_RELEASE_SKIPS ++ publisher
        // names`, which overlap (e.g. `homebrew`, `cargo` appear in both), so a
        // raw join prints each shared token twice. De-dup the hint — a consumer
        // (or the action's skip-token generator) reading "Valid options" should
        // see one clean vocabulary, not a confusing list with repeats.
        Err(format!(
            "invalid --skip value(s): {}. Valid options: {}",
            invalid.join(", "),
            dedup_preserving_order(valid.iter().copied()).join(", "),
        ))
    }
}

/// Collect an iterator of string slices, dropping later duplicates while keeping
/// first-seen order — used so the `--skip` error hint lists each valid token
/// once even though its source set unions overlapping vocabularies.
fn dedup_preserving_order<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|s| seen.insert(*s)).collect()
}

pub struct ContextOptions {
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
    pub skip_stages: Vec<String>,
    /// `--publishers`: per-publisher allowlist. Empty means "no allowlist" —
    /// every publisher runs (subject to `skip_stages`). Non-empty restricts
    /// the publish stage to exactly the named publishers. Entries are
    /// canonical publisher names (`Publisher::name()`, e.g. `npm`, `cargo`).
    /// Orthogonal to `selected_crates` (which scopes crates, not publishers)
    /// and to `skip_stages` (the unified denylist, which always wins — see
    /// [`Context::publisher_deselected`]).
    pub publisher_allowlist: Vec<String>,
    pub selected_crates: Vec<String>,
    pub token: Option<String>,
    /// Maximum number of parallel build jobs (minimum 1).
    pub parallelism: usize,
    /// When set, build only for this single host target triple.
    pub single_target: Option<String>,
    /// Path to a custom release notes file (overrides changelog).
    pub release_notes_path: Option<PathBuf>,
    /// When true, abort immediately on first error during publishing.
    pub fail_fast: bool,
    /// Partial build target for split/merge mode. When set, the build stage
    /// filters targets to only those matching this partial target.
    pub partial_target: Option<PartialTarget>,
    /// When true, running with `--merge` flag (merging artifacts from split builds).
    pub merge: bool,
    /// `--publish-only`: load artifacts from a preserved dist (written
    /// by `anodize check determinism --preserve-dist=...`) and run
    /// only the sign + publish pipeline. The CLI dispatcher uses this
    /// flag in `setup_env` to defer the GitHub-token check to the
    /// config-derived environment preflight (the github-release
    /// publisher's token ladder plus the sign stage's `KeyEnv`
    /// requirements), which validates token and sign-key material in
    /// one collect-all pass and bails fail-closed on missing values.
    /// Without this deferral, `setup_env`'s token check would fire FIRST
    /// and pre-empt that richer, per-publisher preflight.
    pub publish_only: bool,
    /// `--preflight-secrets`: a check-only secrets gate. Like
    /// [`Self::publish_only`], it defers `setup_env`'s GitHub-token hard
    /// error to the config-derived environment preflight (run in
    /// `SecretsOnly` scope), which validates the token ladder alongside
    /// every other runner-agnostic credential and then exits with zero
    /// mutations. Without this deferral, `setup_env` would bail on the
    /// missing token before the secrets gate could report the full set.
    pub preflight_secrets: bool,
    /// Explicit project root directory. When set, stages use this instead of
    /// discovering the repo root via `git rev-parse --show-toplevel`.
    pub project_root: Option<PathBuf>,
    /// Strict mode: configured features that would silently skip become errors.
    pub strict: bool,
    /// `--strict-preflight`: preflight-scoped strictness. Promotes
    /// indeterminate publisher-state / probe outcomes (Unknown state, 5xx /
    /// rate-limit / network failure / undeterminable permissions) to hard
    /// blockers without widening the global `--strict` semantics. Effective
    /// preflight strictness ([`Context::preflight_is_strict`]) ORs this with
    /// `strict` and the config-level `preflight.strict`.
    pub strict_preflight: bool,
    /// `--resume-release`: opt-in to continue into a release left over from
    /// a prior failed attempt. Bypasses the leftover-assets pre-check that
    /// bails when an existing release already has assets and
    /// `replace_existing_artifacts` is false.
    pub resume_release: bool,
    /// `--replace-existing`: CLI override that forces
    /// `release.replace_existing_artifacts: true` regardless of config.
    /// The release stage ORs this with the config value.
    pub replace_existing_artifacts: bool,
    /// `--no-post-publish-poll`: skip post-publish polling for the
    /// chocolatey moderation queue and the winget PR validation pipeline.
    /// When `true`, the polling runner emits `PostPublishStatus::NotPolled`
    /// (pending immediately) for every publisher rather than waiting on a
    /// terminal state. Lets CI users with no patience for long-running
    /// waits opt out without scattering `post_publish_poll.enabled: false`
    /// across every publisher block.
    pub skip_post_publish_poll: bool,
    /// Whether the publisher dispatcher gates irreversible Submitter
    /// publishers (chocolatey, winget, AUR-source, krew, snapcraft) on
    /// the success of every required Assets/Manager publisher that ran
    /// before them. `None` defaults to `Some(true)` (gate on). The CLI
    /// flag `--no-gate-submitter` flips this to `Some(false)`. See
    /// `stage-publish::dispatch::DispatchOptions::gate_submitter` for
    /// the gating mechanics.
    pub gate_submitter: Option<bool>,
    /// `--rollback=<none|best-effort>`: post-publish rollback policy.
    /// `None` means "resolve from preflight state at dispatch time"
    /// (best-effort when preflight ran clean, none otherwise with a
    /// warn). Consumed by the rollback-dispatch task.
    pub rollback_mode: Option<RollbackMode>,
    /// `--simulate-failure=<publisher>` (repeatable, hidden, env-gated
    /// behind `ANODIZE_TEST_HARNESS=1`): names of publishers whose
    /// `run()` should be skipped and a synthetic `Failed("simulated
    /// failure: <name>")` recorded in the report instead. Lets the
    /// failure-mode test harness exercise gate / rollback / report
    /// paths deterministically without monkey-patching production
    /// publisher code.
    pub simulate_failure_publishers: Vec<String>,
    /// `--rollback-only`: skip publish; re-attempt rollback from a
    /// prior run report. Requires `from_run` to identify which prior
    /// run's `report.json` to load. The actual replay logic lands in
    /// a follow-up task; this field is plumbed so the flag is visible
    /// in `--help` today.
    pub rollback_only: bool,
    /// `--allow-rerun`: force `PublishStage::run` to proceed even
    /// when a prior `report.json` exists for the current `run_id`.
    /// The default (false) refuses re-runs to prevent PR-based
    /// publishers (homebrew / scoop / nix / krew / MCP) from
    /// duplicating their pull requests against the same tag.
    ///
    /// Operators recovering from a partial failure should prefer
    /// `--rollback-only --from-run=<id>` (which has its own
    /// idempotency guard via `dist/run-<id>/rollback.json`). The
    /// rerun flag is an escape hatch for advanced cases where the
    /// operator has manually verified no duplicate-publish risk
    /// exists.
    ///
    /// Audit ref: 2026-05-15 release-resilience-review finding I4.
    pub allow_rerun: bool,
    /// `--show-skipped`: surface the per-crate "no `<publisher>` config
    /// block" skip lines at default verbosity. In workspace mode every
    /// PR-based publisher (homebrew / nix / scoop / aur / winget / krew /
    /// chocolatey) visits every selected crate and skips the ones whose
    /// config lacks its block; at default verbosity those no-op skips are
    /// routed to debug (invisible unless `--debug`) so they do not bury the
    /// real output. Setting this flag forces them back to status — the
    /// diagnostic escape hatch for "why didn't publisher X run for crate Y?".
    pub show_skipped: bool,
    /// `--from-run=<id>`: prior run id whose `report.json` to load
    /// when running in `--rollback-only` mode. clap enforces the
    /// `requires = "rollback_only"` relationship at parse time.
    pub from_run: Option<String>,
    /// `--allow-nondeterministic <name>=<reason>` (repeatable):
    /// runtime non-determinism opt-outs for specific artifacts. The
    /// determinism stage suppresses its non-determinism error for
    /// any matching artifact name, recording the supplied reason in
    /// the report. Mutually exclusive with `--strict` at the clap
    /// layer.
    pub runtime_nondeterministic_allowlist: Vec<(String, String)>,
    /// `--summary-json=<path>`: when set, the per-publisher run
    /// summary is written to this path. Consumed by the run-summary
    /// task.
    pub summary_json_path: Option<PathBuf>,
    /// `--allow-ai-failure`: when true, a failure inside the
    /// `changelog.ai` enhancement step (transport, non-2xx, parse) is
    /// logged as a warning and the pre-AI release notes are kept
    /// verbatim. Default `false` (fail-closed) follows the conventional
    /// "any hook failure aborts" pattern: a silent fall-back to the
    /// raw notes ships the wrong body without the operator noticing.
    pub allow_ai_failure: bool,
    /// `changelog --from <ref>`: explicit lower bound (range start) for
    /// changelog commit collection. When set, the changelog stage uses this
    /// ref as the previous tag instead of auto-discovering the latest matching
    /// tag. A dedicated option (rather than the always-auto-populated
    /// `PreviousTag` template var) so a full release run's per-crate
    /// auto-discovery is never overridden — only an explicit `--from` is.
    pub changelog_from: Option<String>,
    /// `changelog ..` / `changelog ..<ref>`: an explicit empty lower bound,
    /// meaning "from the beginning of history" with no auto-discovered
    /// previous tag. When `true`, the changelog stage skips tag
    /// auto-discovery entirely so the range covers all reachable commits up
    /// to the upper bound — distinguishing the explicit empty-from form from
    /// an omitted range (which still resolves to the last release tag).
    pub changelog_full_history: bool,
    /// `changelog <from>..<to>` / `changelog <tag>`: an explicit UPPER bound
    /// (range end) for changelog commit collection. When set, the changelog
    /// stage walks `<from>..<to>` instead of `<from>..HEAD`, so commits AFTER
    /// `<to>` are excluded. A dedicated option (rather than the always-populated
    /// `Tag` template var) so the pending / snapshot window — where `Tag`
    /// resolves to the latest EXISTING tag yet the range must still run to
    /// HEAD — is never silently bounded to that tag. `None` keeps the upper
    /// bound at `HEAD` (the pending window since the last release).
    pub changelog_to: Option<String>,
    /// Marks the run as the standalone `changelog --format release-notes`
    /// LOCAL preview, NOT the `release`/`tag` pipeline. The standalone command
    /// is an inspection tool: it must render the pending window from local git
    /// with no release-time preconditions, so this flag relaxes three guards
    /// that are correct for a real release but wrong for a preview:
    ///   - the tag-must-point-at-HEAD + dirty-tree bails in
    ///     `resolve_git_context` (a preview must not require a checkout or a
    ///     clean tree),
    ///   - the snapshot-skip config gate in the changelog stage (a preview
    ///     must render without `changelog.snapshot: true`),
    ///   - the `use: github-native` branch (a preview renders from local git
    ///     instead of requiring a token / emitting empty bodies).
    ///
    /// ONLY the standalone changelog command sets this; the release/tag
    /// pipelines leave it `false` so their guards stay fully intact.
    pub changelog_preview: bool,
    /// Marks the run as the standalone `anodizer notify` command — a
    /// side-channel that sends a one-off message through the configured
    /// announce integrations, NOT part of the `release` pipeline.
    ///
    /// notify is routinely invoked as an `on_error:` hook AFTER a release has
    /// failed mid-flight, when the working tree is dirty (partial `dist/`,
    /// in-flight writeback) and HEAD may not sit on the release tag. A
    /// notification must never be blocked by repo state — losing the alert is
    /// the worst outcome — so this flag relaxes the three release-time git
    /// preconditions in `resolve_git_context` that are correct for a real
    /// release but wrong for a notification: the no-tag bail (falls back to the
    /// `v0.0.0` synthetic tag), the tag-must-point-at-HEAD bail, and the
    /// dirty-tree bail. ONLY the notify command sets this; every release/tag
    /// path leaves it `false` so their guards stay intact.
    pub notify: bool,
    /// `--allow-snapshot-publish`: downgrade the publish stage's non-release
    /// version guard from a hard bail to a warning.
    ///
    /// By default the publish, blob, and announce stages REFUSE to ship a
    /// non-release version (snapshot / dirty / `0.0.0`-sentinel — see
    /// [`crate::version::guard_release_version`] /
    /// [`crate::version::is_release_version`]) to an external, often
    /// irreversible, channel. The canonical accident this prevents: a CI run
    /// that resolved `0.0.0~SNAPSHOT-<sha>` and pushed it to a package
    /// registry. This flag is the deliberate opt-in for the legitimate
    /// "publish a snapshot to a private channel" case; it is the ONLY thing
    /// required to opt in (the version is not re-stated). Default `false`
    /// (fail-closed).
    pub allow_snapshot_publish: bool,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            snapshot: false,
            nightly: false,
            dry_run: false,
            quiet: false,
            verbose: false,
            debug: false,
            skip_stages: Vec::new(),
            publisher_allowlist: Vec::new(),
            selected_crates: Vec::new(),
            token: None,
            parallelism: 4,
            single_target: None,
            release_notes_path: None,
            fail_fast: false,
            partial_target: None,
            merge: false,
            publish_only: false,
            preflight_secrets: false,
            project_root: None,
            strict: false,
            strict_preflight: false,
            resume_release: false,
            replace_existing_artifacts: false,
            skip_post_publish_poll: false,
            gate_submitter: None,
            rollback_mode: None,
            simulate_failure_publishers: Vec::new(),
            rollback_only: false,
            allow_rerun: false,
            show_skipped: false,
            from_run: None,
            runtime_nondeterministic_allowlist: Vec::new(),
            summary_json_path: None,
            allow_ai_failure: false,
            changelog_from: None,
            changelog_full_history: false,
            changelog_to: None,
            changelog_preview: false,
            notify: false,
            allow_snapshot_publish: false,
        }
    }
}

/// Stage→stage handoff state produced by stages and consumed by later
/// stages (as opposed to `config` / `options` which are pipeline inputs,
/// or `artifacts` which has its own registry). The changelog stage
/// writes here, the release stage reads here.
#[derive(Debug, Default)]
pub struct StageOutputs {
    /// Set by the changelog stage when `use: github-native` is configured.
    /// The release stage reads this to set `generate_release_notes(true)`
    /// on the GitHub API.
    pub github_native_changelog: bool,
    /// Per-crate rendered changelog body, keyed by crate name.
    pub changelogs: HashMap<String, String>,
    /// Rendered `changelog.header` value, populated by the changelog stage.
    /// The release stage uses it as a fallback when `release.header` is
    /// unset so YAML-configured changelog headers reach the GitHub release
    /// body (the release-header content-loading behaviour).
    pub changelog_header: Option<String>,
    /// Rendered `changelog.footer` value, populated by the changelog stage.
    /// Same fallback semantics as `changelog_header`.
    pub changelog_footer: Option<String>,
    /// Per-publisher post-publish polling results, written by the publish
    /// stage's chocolatey / winget polling fan-out and consumed by the
    /// release-summary renderer. Stored as opaque JSON to keep core free
    /// of stage-publish types (the `PostPublishResult` type lives in
    /// `anodizer-stage-publish::post_publish::status` and serializes
    /// stably). Empty when polling was disabled or no eligible
    /// publishers ran.
    pub post_publish_results: Vec<serde_json::Value>,
}

/// Callback that re-runs release-content verification against the already
/// published reversible surface and reports whether it passed. Stored on
/// [`Context`] so the publish dispatcher can gate one-way-door publishers on
/// a fresh verify without `stage-publish` depending on `stage-verify-release`.
/// `Arc` so it can be cheaply cloned out of `&mut Context` before invocation.
pub type VerifyGate = std::sync::Arc<dyn Fn(&mut Context) -> anyhow::Result<bool> + Send + Sync>;

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    /// Stage→stage handoff outputs (changelog text, header/footer, etc.).
    pub stage_outputs: StageOutputs,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
    /// The resolved SCM token type (GitHub, GitLab, or Gitea).
    pub token_type: ScmTokenType,
    /// Aggregated skips from per-sub-config loops (signs, docker_signs,
    /// publishers, …). Drained by the pipeline runner at end-of-pipeline so
    /// the summary shows what was intentionally skipped — mirroring
    /// the skip-memento pattern. The inner `Arc<Mutex<…>>`
    /// lets parallel stage workers contribute without extra plumbing.
    pub skip_memento: crate::pipe_skip::SkipMemento,
    /// Per-expectation skips recorded by the emission-validate pass on a
    /// target-restricted build (an expectation whose target subset was not
    /// built in this run, or a cross-platform aggregate with no eligible
    /// artifact). Kept SEPARATE from [`Self::skip_memento`] on purpose: that
    /// memento is drained into the default-visible end-of-pipeline summary,
    /// while these skips surface only as verbose lines plus an aggregate
    /// count in the stage's one RESULT line — a sharded run would otherwise
    /// print one summary line per unbuilt-target expectation.
    pub emission_skips: crate::pipe_skip::SkipMemento,
    /// Trait-based publisher dispatch report, set by `PublishStage::run`
    /// when the per-publisher dispatcher finishes. `None` until the
    /// publish stage executes (or when publishing is skipped entirely
    /// via snapshot mode / `--skip=publish`). Downstream stages
    /// (SnapcraftPublishStage, AnnounceStage, future Submitter-group
    /// stages) consult this to apply the submitter-gate / announce-gate
    /// rules — see `PublishReport::any_failed`.
    pub publish_report: Option<PublishReport>,
    /// Whether `PublishStage::run` entered its body this run. Set before
    /// the pre-dispatch guards (rerun refusal, runtime allowlist), so a
    /// guard abort leaves this `true` with `publish_report` still `None`
    /// — the summary placeholder row uses the pair to distinguish
    /// "publish skipped" from "publish aborted before dispatch".
    pub publish_attempted: bool,
    /// Verify-release verdict, set by `VerifyReleaseStage::run` immediately
    /// before it returns (clean pass OR `bail!`). `None` until the gate runs
    /// its checks — it stays `None` on the disabled / skipped / dry-run /
    /// snapshot early-returns, where no published release exists to verify.
    ///
    /// Read by the run-summary builder so the end-of-pipeline Summary states
    /// the verify-release outcome on a SEPARATE axis from the publisher rows:
    /// the gate runs after the irreversible publish, so the publishes still
    /// read `succeeded` while this slot records whether the published release
    /// has unverified defects to investigate.
    pub verify_release: Option<VerifyReleaseSummary>,
    /// Pre-submitter verify-release gate, installed once by the CLI's
    /// pipeline-composition layer right after construction (never by a
    /// stage). Invoked by `stage-publish`'s dispatcher immediately before
    /// the first Submitter-group (one-way-door) publisher would run:
    /// `Ok(true)` clears the gate, `Ok(false)` or `Err` blocks every
    /// Submitter-group publisher for the run with
    /// `SkipReason::VerifyGateBlocked`.
    ///
    /// A plain closure field, not a stage, because `stage-publish` cannot
    /// depend on `stage-verify-release` (the dependency runs the other way:
    /// `stage-verify-release` already depends on `stage-publish` for its
    /// terminal landing checks) — routing the call through `Context`, which
    /// every stage crate depends on, avoids the cycle without inventing a
    /// second verify taxonomy. `Arc` (not a bare `Box`) so the field can be
    /// cheaply cloned out of `&mut Context` before being invoked with
    /// `&mut Context`.
    pub verify_gate: Option<VerifyGate>,
    /// SOURCE_DATE_EPOCH seed + non-determinism allow-list state for the
    /// run. `None` until a stage (typically `BuildStage`) seeds it from
    /// `resolve_reproducible_epoch(commit_timestamp)`; downstream stages
    /// (`stage-sbom`, `stage-archive`, `stage-sign`) read `sde` to derive
    /// deterministic timestamps. Lazy-init by design: tests and snapshot
    /// runs without a clean commit can still proceed.
    pub determinism: Option<crate::DeterminismState>,
    /// Per-publisher outcome override published by `Publisher::run` when
    /// the artifact reached a non-`Succeeded` terminal state but `run`
    /// still returned `Ok` (e.g. chocolatey moderation skip,
    /// winget/krew/homebrew PR-already-exists skip). Dispatch consumes
    /// this slot via `take_pending_outcome()` immediately after `run`
    /// returns Ok so the per-publisher row in the summary table reads
    /// `pending-moderation` / `pending-validation` instead of
    /// `succeeded`. The slot is single-shot: any unread value is
    /// cleared at the start of every `run` call.
    pub pending_outcome: Option<crate::PublisherOutcome>,
    /// Partial [`PublishEvidence`] published by `Publisher::run` BEFORE it
    /// returned `Err`, so a publisher that did irreversible work for the
    /// first N items and then failed on item N+1 can still hand the
    /// rollback path the authoritative record of what actually went live.
    ///
    /// The cargo publisher is the motivating case: a multi-crate publish
    /// that succeeds on crate A then fails on crate B must yank A. On the
    /// `Ok` path `run` returns its evidence directly; on the `Err` path
    /// dispatch consumes this slot via [`Context::take_pending_evidence`]
    /// and records it on the failed publisher's report row so rollback has
    /// something to act on. Single-shot — drained at the start of every
    /// `run` and cleared on the `Ok` path.
    pub pending_evidence: Option<crate::PublishEvidence>,
    /// Distinct set of crate names the build stage actually built — i.e.
    /// those that had at least one in-scope build (or `copy_from`) job after
    /// target resolution. `None` until [`BuildStage`] runs (e.g. merge mode,
    /// which pre-loads artifacts and never invokes the build stage).
    ///
    /// Read by the binary-artifact guard to distinguish "configured a
    /// binary-requiring surface but legitimately had no in-scope target in
    /// this shard" (skip) from "was built yet produced no binary" (a real
    /// mis-scope to fail on). Populated via [`Context::set_built_crate_names`]
    /// and read via [`Context::built_crate_names`].
    built_crate_names: Option<std::collections::HashSet<String>>,
    /// Injectable environment-variable source. Defaults to
    /// [`ProcessEnvSource`] (reads `std::env::var`). Tests inject a
    /// [`MapEnvSource`](crate::MapEnvSource) via
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env)
    /// so deterministic branches can be exercised without mutating the
    /// process env. Read through [`Context::env_var`]; replace via
    /// [`Context::set_env_source`].
    env_source: Arc<dyn EnvSource>,
    /// Live handle to the secret-redaction table shared with every
    /// [`StageLogger`] this context has ever produced via [`Context::logger`]
    /// (each gets a clone of the same `Arc<Mutex<_>>` cell). Refreshed from
    /// [`Context::env_for_redact`] by [`Context::refresh_secret_env`] at every
    /// `env_source` mutation point (`set_env_source`, `set_env_source_arc`,
    /// `begin_cargo_trusted_publishing`, `end_cargo_trusted_publishing`), so a
    /// logger constructed BEFORE a mid-run credential mint — e.g. crates.io
    /// Trusted Publishing minting `CARGO_REGISTRY_TOKEN` into `env_source`
    /// partway through `publish_to_cargo` — still redacts it: `StageLogger::
    /// redact` reads this cell live rather than a frozen construction-time
    /// snapshot.
    secret_env: crate::log::RedactionEnv,
    /// Live crates.io Trusted-Publishing overlay state, set for the duration
    /// of a cargo publish that minted a short-lived token via OIDC. Holds the
    /// minted token (the revoke + yank-injection credential) and the base env
    /// source captured before the overlay was installed (restored on
    /// teardown). `None` on the ambient `auth: token` path and outside a
    /// mint. Managed exclusively through
    /// [`Context::begin_cargo_trusted_publishing`] /
    /// [`Context::end_cargo_trusted_publishing`].
    cargo_trusted_publishing: Option<CargoTrustedPublishing>,
    /// Optional in-memory log-capture handle. When `Some`, every logger
    /// produced by [`Context::logger`] attaches it so the test can read
    /// back aggregated counts of `status` / `warn` / etc. calls without
    /// having to intercept stderr.
    ///
    /// Gated behind the `test-helpers` Cargo feature — production
    /// binaries do not carry the field at all.
    #[cfg(feature = "test-helpers")]
    pub log_capture: Option<crate::log::LogCapture>,
    /// Runtime-togglable strict-render flag, distinct from the user's global
    /// `--strict` (`options.strict`). The pre-publish guard flips this on for
    /// the duration of its in-memory render pass (via [`Context::set_render_strict`])
    /// so EVERY publisher/announce template it renders propagates its error
    /// instead of falling back to the raw string — turning a swallowed
    /// broken-template warning into a release-blocking abort BEFORE any
    /// irreversible publisher fires. Production publish leaves it `false`, so
    /// dry-run / snapshot / nightly stay lenient (warn + raw fallback).
    ///
    /// A `Cell` (not a plain `bool`) because the render path holds only a
    /// shared `&Context`: the guard sets it through its `&mut Context`, then
    /// the deep render helpers toggle-read it through `&Context`.
    /// [`Context::render_is_strict`] ORs this with `is_strict()`, so the user's
    /// global `--strict` also makes every render strict everywhere.
    render_strict: std::cell::Cell<bool>,
    /// When true, announce message BODIES are treated as already-final text and
    /// are NOT run through Tera at send time. Set by `anodizer notify` so an
    /// operator-supplied (possibly untrusted) message — e.g. an on_error error
    /// string — cannot expand an `Env`-reference into a secret when the
    /// provider sends it. Only message bodies are affected; titles and other
    /// templated fields still render normally.
    pub literal_message: bool,
    /// When true (the default), outbound announce message BODIES have
    /// known-secret env values masked before send (same policy as log
    /// redaction). `anodizer notify --allow-secrets` sets this false to send a
    /// secret deliberately over a trusted channel. Only the outbound body is
    /// affected; anodizer's own logs are redacted unconditionally regardless of
    /// this flag.
    pub redact_body: bool,
}

/// Live crates.io Trusted-Publishing overlay state (see the `Context`
/// `cargo_trusted_publishing` field). The `base` is the env source captured
/// before the token overlay was installed, restored verbatim on teardown.
struct CargoTrustedPublishing {
    token: String,
    base: Arc<dyn EnvSource>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        let ctx = Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            stage_outputs: StageOutputs::default(),
            template_vars: vars,
            git_info: None,
            token_type: ScmTokenType::GitHub,
            skip_memento: crate::pipe_skip::SkipMemento::new(),
            emission_skips: crate::pipe_skip::SkipMemento::new(),
            publish_report: None,
            publish_attempted: false,
            verify_release: None,
            verify_gate: None,
            determinism: None,
            pending_outcome: None,
            pending_evidence: None,
            built_crate_names: None,
            env_source: Arc::new(ProcessEnvSource),
            secret_env: Arc::new(Mutex::new(Vec::new())),
            cargo_trusted_publishing: None,
            #[cfg(feature = "test-helpers")]
            log_capture: None,
            render_strict: std::cell::Cell::new(false),
            literal_message: false,
            redact_body: true,
        };
        ctx.refresh_secret_env();
        ctx
    }

    /// Redact known-secret env values from outbound announce text, using the
    /// same combined env (template engine env + process env) and the same
    /// policy as log redaction. Always redacts; gating on `redact_body` is the
    /// caller's responsibility (see `render_message_with_default`).
    pub fn redact(&self, s: &str) -> String {
        crate::redact::with_env(s, &self.env_for_redact())
    }

    /// Read an environment variable through the injected source.
    ///
    /// Production reads `std::env::var(name).ok()`. Tests inject a
    /// [`MapEnvSource`](crate::MapEnvSource) via
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env)
    /// so deterministic branches can be exercised without mutating the
    /// process env.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.env_source.var(name)
    }

    /// Replace the injected environment-variable source.
    ///
    /// Production migration code uses this when wrapping an
    /// already-constructed context; tests reach this indirectly through
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env).
    pub fn set_env_source<S: EnvSource + 'static>(&mut self, src: S) {
        self.env_source = Arc::new(src);
        self.refresh_secret_env();
    }

    /// Replace the injected environment-variable source with an already-boxed
    /// `Arc<dyn EnvSource>`. Used to RESTORE a previously captured base source
    /// after a temporary overlay (see
    /// [`Context::begin_cargo_trusted_publishing`]) without re-wrapping it.
    pub fn set_env_source_arc(&mut self, src: Arc<dyn EnvSource>) {
        self.env_source = src;
        self.refresh_secret_env();
    }

    /// Overlay a minted crates.io Trusted-Publishing token as
    /// `CARGO_REGISTRY_TOKEN` for the cargo publish+rollback lifecycle.
    ///
    /// The current env source is captured as the base, then wrapped in a
    /// [`LayeredEnvSource`](crate::LayeredEnvSource) that overrides
    /// `CARGO_REGISTRY_TOKEN` with `token`. This makes the token visible to
    /// env-driven paths that read through [`Context::env_source`] — notably
    /// the rollback scope-availability gate — so a partial OIDC publish can
    /// still yank, even though no ambient token exists. The token is also
    /// retained as a marker so a later `rollback()` knows a minted token is
    /// live and must be revoked after the yank.
    ///
    /// Paired with [`Context::end_cargo_trusted_publishing`], which restores
    /// the base source and returns the token for best-effort revocation.
    pub fn begin_cargo_trusted_publishing(&mut self, token: String) {
        let base = self.env_source_arc();
        self.env_source = Arc::new(crate::env_source::LayeredEnvSource::new(
            Arc::clone(&base),
            [("CARGO_REGISTRY_TOKEN".to_string(), token.clone())],
        ));
        self.cargo_trusted_publishing = Some(CargoTrustedPublishing { token, base });
        self.refresh_secret_env();
    }

    /// The minted crates.io Trusted-Publishing token, if an overlay is active.
    /// `rollback()` reads this to learn (i) that the yank must inject a minted
    /// token, and (ii) that the token must be revoked once the yank completes.
    pub fn cargo_trusted_publishing_token(&self) -> Option<&str> {
        self.cargo_trusted_publishing
            .as_ref()
            .map(|s| s.token.as_str())
    }

    /// Tear down the Trusted-Publishing overlay: restore the captured base env
    /// source, drop the marker, and return the minted token so the caller can
    /// revoke it (best-effort). Returns `None` when no overlay is active (the
    /// `auth: token` / ambient path never mints, so its long-lived token is
    /// neither overlaid nor revoked).
    pub fn end_cargo_trusted_publishing(&mut self) -> Option<String> {
        let state = self.cargo_trusted_publishing.take()?;
        self.env_source = state.base;
        self.refresh_secret_env();
        Some(state.token)
    }

    /// Borrow the injected environment-variable source as a trait
    /// object so callers can pass it into helpers that take
    /// `&dyn EnvSource` / `&E: EnvSource + ?Sized` without re-binding
    /// each var through [`Context::env_var`].
    pub fn env_source(&self) -> &dyn EnvSource {
        self.env_source.as_ref()
    }

    /// Clone the injected environment-variable source as an `Arc` so
    /// callers can move it into a `tokio::spawn` future or any other
    /// `'static` closure. Production-default value is
    /// [`ProcessEnvSource`]; tests may replace it via
    /// [`Context::set_env_source`].
    pub fn env_source_arc(&self) -> Arc<dyn EnvSource> {
        Arc::clone(&self.env_source)
    }

    /// Attach an in-memory log-capture sink so every logger derived from
    /// this context via [`Context::logger`] records to it. Intended for
    /// tests; production callers leave this `None`.
    ///
    /// Gated behind the `test-helpers` Cargo feature.
    #[cfg(feature = "test-helpers")]
    pub fn with_log_capture(&mut self, capture: crate::log::LogCapture) {
        self.log_capture = Some(capture);
    }

    /// Publisher-facing override: when `Publisher::run` returns `Ok`
    /// but the terminal outcome is something other than `Succeeded`
    /// (chocolatey moderation skip, winget/krew/homebrew
    /// PR-already-exists skip, …) call this before returning so
    /// dispatch records the correct `PublisherOutcome` on the report.
    /// Without this, dispatch defaults to `Succeeded` on any Ok and
    /// the summary table silently misreports the skip as success.
    pub fn record_publisher_outcome(&mut self, outcome: crate::PublisherOutcome) {
        self.pending_outcome = Some(outcome);
    }

    /// Dispatch-side consumer: take the pending outcome override (if
    /// any) recorded by the publisher's `run`. Single-shot — the slot
    /// is empty after this call.
    pub fn take_pending_outcome(&mut self) -> Option<crate::PublisherOutcome> {
        self.pending_outcome.take()
    }

    /// Publisher-side recorder: stash the partial evidence accumulated
    /// before a failing `run` returns `Err`, so dispatch can attach it to
    /// the failed report row and rollback has the authoritative record of
    /// what went live. See [`Context::pending_evidence`].
    pub fn record_pending_evidence(&mut self, evidence: crate::PublishEvidence) {
        self.pending_evidence = Some(evidence);
    }

    /// Dispatch-side consumer: take the partial evidence (if any) a
    /// publisher recorded before failing. Single-shot — empty after this
    /// call.
    pub fn take_pending_evidence(&mut self) -> Option<crate::PublishEvidence> {
        self.pending_evidence.take()
    }

    /// Borrow the publisher dispatch report set by `PublishStage::run`,
    /// or `None` if the publish stage hasn't run yet (or was skipped).
    pub fn publish_report(&self) -> Option<&PublishReport> {
        self.publish_report.as_ref()
    }

    /// Whether the publish stage entered its body this run (even if it
    /// aborted before dispatching any publisher).
    pub fn publish_attempted(&self) -> bool {
        self.publish_attempted
    }

    /// Record that the publish stage entered its body. Called by
    /// `PublishStage::run` ahead of its pre-dispatch guards so guard
    /// aborts are distinguishable from a skipped stage.
    pub fn set_publish_attempted(&mut self) {
        self.publish_attempted = true;
    }

    /// Store the publisher dispatch report. Overwrites any prior value.
    ///
    /// Written by the publish stage during a normal release run; rehydrated by
    /// `--announce-only` from the on-disk `<dist>/run-<id>/report.json` so the
    /// announce stage sees an equivalent context without re-publishing.
    pub fn set_publish_report(&mut self, r: PublishReport) {
        self.publish_report = Some(r);
    }

    /// Borrow the set of crate names the build stage actually built, or
    /// `None` if the build stage has not run in this pipeline (merge mode).
    pub fn built_crate_names(&self) -> Option<&std::collections::HashSet<String>> {
        self.built_crate_names.as_ref()
    }

    /// Record the distinct crate names that received at least one in-scope
    /// build job. Called once by the build stage after job planning.
    pub fn set_built_crate_names(&mut self, names: std::collections::HashSet<String>) {
        self.built_crate_names = Some(names);
    }

    /// Record an intentional skip from a per-sub-config loop
    /// (`signs`, `docker_signs`, `publishers`, …). `stage` identifies the
    /// owning stage, `label` identifies the sub-config (id / name / index),
    /// `reason` is short user-facing text. Duplicate (stage, label, reason)
    /// tuples are dropped on insert so a per-artifact inner loop cannot emit
    /// N copies of the same skip message.
    pub fn remember_skip(&self, stage: &str, label: &str, reason: &str) {
        self.skip_memento.remember(stage, label, reason);
    }

    pub fn template_vars(&self) -> &TemplateVars {
        &self.template_vars
    }

    pub fn template_vars_mut(&mut self) -> &mut TemplateVars {
        &mut self.template_vars
    }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    /// Render `template` with the FULL version-derived var set (`Version`, `Tag`,
    /// `Major`/`Minor`/`Patch`, `RawVersion`, `Prerelease`, `BuildMetadata`,
    /// `Base`) re-derived from `version`/`tag` rather than the context's own git
    /// version — used by promotion to reconstruct the immutable tag a prior
    /// release pushed for a specific `--version`. Overriding only `Version`+`Tag`
    /// would leave `{{ .Major }}` etc. resolving to the CONTEXT version, so a
    /// `{{ .Major }}.{{ .Minor }}.{{ .Patch }}` tag template would render the
    /// wrong source tag whenever the context version differs from the target.
    ///
    /// A non-semver `version` (no parse) falls back to overriding `Version`+`Tag`
    /// and BLANKING the seven semver-part vars (`RawVersion`, `Base`, `Major`,
    /// `Minor`, `Patch`, `Prerelease`, `BuildMetadata`), so a semver-part template
    /// cannot silently resolve the context version — it renders empty instead of
    /// inheriting the cloned context's parts. Does not mutate `self`.
    pub fn render_template_for_version(
        &self,
        template: &str,
        version: &str,
        tag: &str,
    ) -> anyhow::Result<String> {
        let mut vars = self.template_vars.clone();
        match crate::git::parse_semver(version) {
            Ok(semver) => set_version_vars(&mut vars, &semver, tag),
            Err(_) => {
                vars.set("Version", version);
                vars.set("Tag", tag);
                // Blank the semver-derived vars `set_version_vars` writes so a
                // `{{ .Major }}`-style template renders empty rather than
                // inheriting the cloned context version's parts (a false match).
                for key in [
                    "RawVersion",
                    "Base",
                    "Major",
                    "Minor",
                    "Patch",
                    "Prerelease",
                    "BuildMetadata",
                ] {
                    vars.set(key, "");
                }
            }
        }
        crate::template::render(template, &vars)
    }

    /// Render a template if present, returning `None` for `None` input.
    pub fn render_template_opt(&self, template: Option<&str>) -> anyhow::Result<Option<String>> {
        template.map(|t| self.render_template(t)).transpose()
    }

    /// Evaluate a `skip` field, logging at INFO level when it resolves to true.
    ///
    /// Returns `Ok(false)` when `skip` is `None` or evaluates falsy. On
    /// truthy, writes `"{label} skipped"` via `log.status` and returns
    /// `Ok(true)`. A malformed `skip:` template propagates as `Err` so the
    /// caller fails fast — silently treating a render error as "not skipped"
    /// (the prior behavior) shipped configs that the user thought would
    /// suppress a stage but actually ran it.
    pub fn skip_with_log(
        &self,
        skip: &Option<crate::config::StringOrBool>,
        log: &StageLogger,
        label: &str,
    ) -> anyhow::Result<bool> {
        let Some(d) = skip else {
            return Ok(false);
        };
        let should_skip = d
            .try_evaluates_to_true(|s| self.render_template(s))
            .with_context(|| format!("evaluate skip expression for {label}"))?;
        if should_skip {
            log.status(&format!("{} skipped", label));
        }
        Ok(should_skip)
    }

    /// Whether `stage_name` (or a publisher name — the skip list is unified) is
    /// in the operator's `--skip` denylist.
    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    /// Whether the named publisher is excluded from this run by operator
    /// selection. Combines the two selectors the publish dispatch consults
    /// before running any publisher:
    ///
    /// - `--skip` (`skip_stages`, the UNIFIED denylist holding stage names
    ///   AND publisher names) ALWAYS wins: a publisher named there is
    ///   deselected regardless of any allowlist.
    /// - `--publishers` (`publisher_allowlist`): an EMPTY allowlist deselects
    ///   nothing (every publisher runs); a NON-EMPTY allowlist deselects every
    ///   publisher not listed in it.
    ///
    /// Returns `true` when the publisher should be reported
    /// [`crate::publish_report::SkipReason::Deselected`] instead of dispatched.
    pub fn publisher_deselected(&self, name: &str) -> bool {
        self.should_skip(name)
            || (!self.options.publisher_allowlist.is_empty()
                && !self.options.publisher_allowlist.iter().any(|s| s == name))
    }

    /// Whether ANY of the named publishers survives the operator-selection
    /// filter — the positive dual of [`Self::publisher_deselected`] over a
    /// set. One helper for both registers ("is any consumer selected?" and
    /// its negation "are all consumers deselected?") so callers never
    /// hand-roll De Morgan twins that can drift apart.
    pub fn any_publisher_selected(&self, names: &[&str]) -> bool {
        names.iter().any(|n| !self.publisher_deselected(n))
    }

    /// A distinguished, operator-facing summary line for a deselected
    /// publisher, naming WHICH selector excluded it so the operator can fix
    /// their command. `--skip` always wins, so it is tested first: a publisher
    /// named in both selectors reports the denylist cause.
    ///
    /// Shared by the dispatch chokepoint and the out-of-dispatch publish
    /// stages (blob / snapcraft-publish / docker / docker-sign / announce) so the
    /// "skipped X — excluded via --skip" / "… — not in --publishers allowlist"
    /// wording is identical everywhere a publisher is deselected. Call only
    /// when [`Self::publisher_deselected`] is `true`.
    pub fn deselected_reason(&self, name: &str) -> String {
        let reason = if self.should_skip(name) {
            "excluded via --skip"
        } else {
            "not in --publishers allowlist"
        };
        format!("skipped {name} — {reason}")
    }

    /// Check whether "validate" is in the skip list.
    pub fn skip_validate(&self) -> bool {
        self.should_skip("validate")
    }

    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub fn is_snapshot(&self) -> bool {
        self.options.snapshot
    }

    /// Whether this run builds only a subset of the configured targets — either
    /// a `--split` / `--targets` determinism shard (`partial_target`) or a
    /// host-only `--single-target` build.
    ///
    /// A publisher whose eligible artifact is legitimately absent on a
    /// restricted build (e.g. a Windows-only publisher on a Linux single-target
    /// snapshot) must self-skip its schema validation rather than error: the
    /// artifact lands on another target, not a misconfiguration. On a FULL build
    /// the same absence IS a misconfiguration and must surface. `--single-target`
    /// (`single_target`) is clap-exclusive with `--targets` / `--host-targets`
    /// (which populate `partial_target`), but NOT with `--split` (a split shard
    /// resolves its own `partial_target` from `partial.by` yet may still be
    /// scoped to the host target), so both signals can be set at once; this OR
    /// is the single "restricted build" predicate the per-publisher validators
    /// gate their no-artifact skip on, correct whether one or both are set.
    pub fn is_target_restricted_build(&self) -> bool {
        self.options.partial_target.is_some() || self.options.single_target.is_some()
    }

    /// Whether this run is `anodizer release --publish-only` (publishing a
    /// preserved dist rather than building from source).
    ///
    /// Build-time concerns (notably the `binary_signs:` per-binary signing
    /// loop, whose output is embedded into archives at build time and has no
    /// publish-time consumer) are gated off this in publish-only mode, where
    /// the runner carries only publish-time credentials.
    pub fn is_publish_only(&self) -> bool {
        self.options.publish_only
    }

    pub fn is_strict(&self) -> bool {
        self.options.strict
    }

    /// Effective preflight strictness: the global `--strict`, the scoped
    /// `--strict-preflight`, or the config-level `preflight.strict` — any one
    /// turns it on. Under strict preflight, indeterminate probe outcomes
    /// (Unknown publisher state, 5xx / rate-limit / network failure /
    /// undeterminable permissions) become hard blockers instead of warnings.
    /// Definitive failures keep their required→blocker / optional→warning
    /// severity either way.
    pub fn preflight_is_strict(&self) -> bool {
        self.options.strict || self.options.strict_preflight || self.config.preflight.strict
    }

    /// Toggle the runtime strict-render flag (see the `render_strict` field).
    ///
    /// The pre-publish guard calls this with `true` before its render pass and
    /// restores the prior value after, so render-error swallowing is suppressed
    /// only for that in-memory validation — production publish renders stay
    /// lenient unless the user passed the global `--strict`. Returns the prior
    /// value so the caller can restore it.
    pub fn set_render_strict(&self, on: bool) -> bool {
        self.render_strict.replace(on)
    }

    /// Whether template renders should propagate errors (strict) rather than
    /// warn-and-fall-back-to-raw (lenient).
    ///
    /// True when EITHER the guard's transient `render_strict` flag is set OR the
    /// user passed the global `--strict`, so a malformed publisher/announce
    /// template fails loud under the guard and under `--strict` everywhere.
    pub fn render_is_strict(&self) -> bool {
        self.render_strict.get() || self.is_strict()
    }

    /// In strict mode, return an error. In normal mode, log a warning and continue.
    /// Use this for any situation where a configured feature silently skips.
    pub fn strict_guard(&self, log: &crate::log::StageLogger, msg: &str) -> anyhow::Result<()> {
        if self.options.strict {
            anyhow::bail!("{} (strict mode)", msg);
        }
        log.warn(msg);
        Ok(())
    }

    /// Defense-in-depth helper for upload-style stages.
    ///
    /// Returns `true` (after logging the skip) when the context is in snapshot
    /// mode. Stages that perform external uploads (registries, package indexes,
    /// object storage, snap store, …) call this at entry so they no-op even
    /// when invoked directly without the orchestration layer's auto-skip.
    /// Centralising the check keeps every publish stage consistent and avoids
    /// per-stage copy-paste.
    pub fn skip_in_snapshot(&self, log: &crate::log::StageLogger, stage: &str) -> bool {
        if self.is_snapshot() {
            // The stage name stays in the line: this guard fires on direct
            // stage invocation, where no pipeline section header has named
            // the stage yet.
            log.status(&format!("skipped {stage} — snapshot mode"));
            true
        } else {
            false
        }
    }

    /// Render a template, failing in strict mode on error, or falling back to the raw string.
    pub fn render_template_strict(
        &self,
        template: &str,
        label: &str,
        log: &crate::log::StageLogger,
    ) -> anyhow::Result<String> {
        match self.render_template(template) {
            Ok(rendered) => Ok(rendered),
            Err(e) => {
                if self.options.strict {
                    anyhow::bail!("{}: failed to render template: {} (strict mode)", label, e);
                }
                log.warn(&format!("failed to render template for {}: {}", label, e));
                Ok(template.to_string())
            }
        }
    }

    pub fn is_nightly(&self) -> bool {
        self.options.nightly
    }

    /// Set the `ReleaseURL` template variable.
    ///
    /// Should be called after a GitHub release is created, with the URL of
    /// the created release (e.g. `https://github.com/owner/repo/releases/tag/v1.0.0`).
    pub fn set_release_url(&mut self, url: &str) {
        self.template_vars.set("ReleaseURL", url);
    }

    /// Return the current `Version` template variable, or an empty string if
    /// not yet populated.
    pub fn version(&self) -> String {
        self.template_vars
            .get("Version")
            .cloned()
            .unwrap_or_default()
    }

    /// Reproducible-mtime seed shared by every stage that stamps a build
    /// timestamp into a produced artifact (release archives, source archives,
    /// PyPI wheels + sdists).
    ///
    /// Resolution ladder, single-sourced here so archives and wheels never
    /// pick different timestamps in one run:
    ///
    /// 1. when ANY build in the crate universe is `reproducible: true`, the
    ///    commit timestamp wins outright — a reproducible build pins its own
    ///    output to the commit, so a stray ambient `SOURCE_DATE_EPOCH` must
    ///    not override it;
    /// 2. otherwise `SOURCE_DATE_EPOCH` (the standard reproducibility
    ///    contract, set by the determinism harness on every child), falling
    ///    back to the commit timestamp.
    ///
    /// Returns `None` when neither a commit timestamp nor `SOURCE_DATE_EPOCH`
    /// is available (writers then leave the default wall-clock stamp).
    pub fn resolve_reproducible_mtime(&self) -> Option<u64> {
        let any_reproducible = self.config.crate_universe().into_iter().any(|c| {
            c.builds
                .as_ref()
                .is_some_and(|builds| builds.iter().any(|b| b.reproducible.unwrap_or(false)))
        });
        let commit_ts = self
            .template_vars()
            .get("CommitTimestamp")
            .and_then(|ts| ts.parse::<u64>().ok());
        if any_reproducible {
            commit_ts
        } else {
            self.env_var("SOURCE_DATE_EPOCH")
                .and_then(|s| s.parse::<u64>().ok())
                .or(commit_ts)
        }
    }

    /// Derive the verbosity level from context options.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_flags(self.options.quiet, self.options.verbose, self.options.debug)
    }

    /// Resolve the user's `retry:` block into a concrete [`RetryPolicy`],
    /// applying defaults when `retry:` is unset. Equivalent to
    /// `ctx.config.retry.unwrap_or_default().to_policy()` but centralizes
    /// the lookup so a future refactor can hang validation / clamping off
    /// a single seam.
    pub fn retry_policy(&self) -> crate::retry::RetryPolicy {
        self.config.retry.unwrap_or_default().to_policy()
    }

    /// Resolve the retry wall-clock budget into an absolute deadline anchored at
    /// the moment of this call. Always `Some`: `retry.max_elapsed` when the user
    /// sets it, otherwise [`crate::retry::DEFAULT_MAX_ELAPSED`] (15 min) — so a
    /// publisher that threads this into [`crate::retry::retry_sync_deadline`] /
    /// [`crate::retry::retry_async_deadline`] is bounded by default and the
    /// operator can raise or lower the ceiling with one config field. The
    /// `Option` return lets it feed those engines verbatim (their `None` means
    /// unbounded, reserved for callers with no context). Computed once at the
    /// start of a publish sequence so a long transient storm exits cleanly
    /// (resumable) instead of being SIGKILLed mid-write by the outer job timeout.
    pub fn retry_deadline(&self) -> Option<std::time::Instant> {
        let budget = self
            .config
            .retry
            .unwrap_or_default()
            .max_elapsed_duration()
            .unwrap_or(crate::retry::DEFAULT_MAX_ELAPSED);
        Some(std::time::Instant::now() + budget)
    }

    /// Create a [`StageLogger`] for the given stage name, pre-attached to
    /// the context's env-pairs list so that subprocess stderr / stdout
    /// flowing through [`StageLogger::check_output`] is automatically
    /// redacted. The env list combines the template-engine env
    /// (process + config + `.env` files) and the current `std::env::vars`
    /// snapshot, so any secret value reachable to a hook or subprocess is
    /// available for scrubbing.
    pub fn logger(&self, stage: &'static str) -> StageLogger {
        // Snapshot the current redaction env into the shared cell at
        // construction so a secret injected via `template_vars_mut().set_env`
        // (which has no mutation hook) is captured, matching the historical
        // snapshot-at-`logger()` behavior. The cell stays shared afterward, so
        // a secret minted later through an `env_source` mutation still reaches
        // this logger via `refresh_secret_env` at that mutation point.
        self.refresh_secret_env();
        #[allow(unused_mut)]
        let mut log =
            StageLogger::new(stage, self.verbosity()).with_shared_env(Arc::clone(&self.secret_env));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.log_capture {
            log = log.with_capture_handle(cap.clone());
        }
        log
    }

    /// Build the env-pairs list used to seed every [`StageLogger`] created
    /// via [`Context::logger`]. Combines the template-engine env map
    /// (config env + `.env` file values) with the injected [`EnvSource`]'s
    /// full snapshot ([`EnvSource::vars`]), deduplicating by key
    /// (template-engine values win because they reflect any user
    /// overrides).
    ///
    /// Routes through `self.env_source` — not a raw `std::env::vars()` read
    /// — so [`TestContextBuilder::sealed_env`](crate::test_helpers::TestContextBuilder::sealed_env)'s
    /// documented "never the ambient process environment" promise also
    /// covers log/announce redaction, not just [`Context::env_var`] point
    /// lookups. A hermetic test that seals its env must not have an
    /// unrelated real ambient secret-suffixed var silently mask a literal
    /// fixture substring in the redacted output.
    fn env_for_redact(&self) -> Vec<(String, String)> {
        use std::collections::HashMap;
        let mut map: HashMap<String, String> = self.env_source.vars().into_iter().collect();
        for (k, v) in self.template_vars.all_env() {
            map.insert(k.clone(), v.clone());
        }
        map.into_iter().collect()
    }

    /// Recompute [`Context::env_for_redact`] and publish it into
    /// [`Context::secret_env`], the live cell every [`StageLogger`] produced
    /// by [`Context::logger`] shares. Called at every `env_source` mutation
    /// point so a logger built earlier in the run still redacts a secret
    /// minted afterward (see the `secret_env` field doc for the concrete
    /// crates.io Trusted-Publishing scenario this closes).
    fn refresh_secret_env(&self) {
        let fresh = self.env_for_redact();
        *self.secret_env.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
    }

    /// Populate template variables from `self.git_info`.
    ///
    /// Must be called after `self.git_info` is set. Sets the following vars:
    /// - `Tag`, `Version`, `RawVersion` — tag and version strings
    /// - `Major`, `Minor`, `Patch` — semver components
    /// - `Prerelease` — prerelease suffix (or empty)
    /// - `BuildMetadata` — build metadata from semver tag (or empty)
    /// - `FullCommit`, `Commit` — full commit SHA (`Commit` is alias for `FullCommit`)
    /// - `ShortCommit` — abbreviated commit SHA
    /// - `Branch` — current git branch
    /// - `CommitDate` — ISO 8601 author date of HEAD commit
    /// - `CommitTimestamp` — unix timestamp of HEAD commit
    /// - `IsGitDirty` — "true"/"false"
    /// - `IsGitClean` — "true"/"false" (inverse of `IsGitDirty`)
    /// - `GitTreeState` — "clean"/"dirty"
    /// - `GitURL` — git remote URL
    /// - `Summary` — git describe summary
    /// - `TagSubject` — annotated tag subject or commit subject
    /// - `TagContents` — full annotated tag message or commit message
    /// - `TagBody` — tag message body or commit message body
    /// - `IsSnapshot` — from context options
    /// - `IsNightly` — from context options
    /// - `IsDraft` — "false" (stages may override to "true")
    /// - `IsSingleTarget` — "true"/"false" based on single_target option
    /// - `PreviousTag` — previous matching tag, stripped in monorepo mode (or empty)
    /// - `PrefixedTag` — full tag with monorepo prefix, or tag_prefix-prepended (Pro addition)
    /// - `PrefixedPreviousTag` — full previous tag with prefix (Pro addition)
    /// - `PrefixedSummary` — full summary with prefix (Pro addition)
    /// - `IsRelease` — "true" if not snapshot and not nightly (Pro addition)
    /// - `IsMerging` — "true" if running with --merge flag (Pro addition)
    ///
    /// **Stage-scoped variables** (NOT set here; set per-artifact during stage execution):
    /// - `Binary` — binary name, set by build stage per binary and archive stage per archive
    /// - `ArtifactName` — output artifact filename, set by archive stage after creating each archive
    /// - `ArtifactPath` — absolute path to artifact, set by archive stage after creating each archive
    /// - `ArtifactExt` — artifact file extension (e.g. `.tar.gz`, `.exe`), set alongside ArtifactName
    /// - `ArtifactID` — build config `id` field, set by build stage per build config
    /// - `Os` — target OS, set by archive/nfpm stages per target
    /// - `Arch` — target architecture, set by archive/nfpm stages per target
    /// - `Target` — full target triple (e.g. `x86_64-unknown-linux-gnu`), set alongside Os/Arch
    /// - `Checksums` — combined checksum file contents, set by checksum stage
    pub fn populate_git_vars(&mut self) {
        if let Some(ref info) = self.git_info {
            // The version-derived var block (Tag/Version/RawVersion/Base/Major/
            // Minor/Patch/Prerelease/BuildMetadata) is factored into
            // `set_version_vars` so `render_template_for_version` can re-derive
            // the SAME block for a promotion's target version without drift.
            // Deriving Version/RawVersion from the parsed `SemVer` struct (not
            // `tag.strip_prefix('v')`) handles monorepo tags like `core-v0.3.2`.
            set_version_vars(&mut self.template_vars, &info.semver, &info.tag);
            self.template_vars.set("FullCommit", &info.commit);
            self.template_vars.set("Commit", &info.commit);
            self.template_vars.set("ShortCommit", &info.short_commit);
            self.template_vars.set("Branch", &info.branch);
            self.template_vars.set("CommitDate", &info.commit_date);
            self.template_vars
                .set("CommitTimestamp", &info.commit_timestamp);
            self.template_vars.set_bool("IsGitDirty", info.dirty);
            self.template_vars.set_bool("IsGitClean", !info.dirty);
            self.template_vars
                .set("GitTreeState", if info.dirty { "dirty" } else { "clean" });
            self.template_vars.set("GitURL", &info.remote_url);
            self.template_vars.set("Summary", &info.summary);
            self.template_vars.set("TagSubject", &info.tag_subject);
            self.template_vars.set("TagContents", &info.tag_contents);
            self.template_vars.set("TagBody", &info.tag_body);
            self.template_vars
                .set("PreviousTag", info.previous_tag.as_deref().unwrap_or(""));
            self.template_vars
                .set("FirstCommit", info.first_commit.as_deref().unwrap_or(""));

            // Pro additions: PrefixedTag, PrefixedPreviousTag, PrefixedSummary
            //
            // When monorepo.tag_prefix is configured, the git tag already
            // contains the prefix (e.g. "subproject1/v1.2.3"). In this case:
            //   - Tag = prefix stripped (e.g. "v1.2.3")
            //   - PrefixedTag = full tag (e.g. "subproject1/v1.2.3")
            //   - PrefixedPreviousTag = full previous tag
            //
            // When monorepo is NOT configured, fall back to the original
            // behavior: prepend tag.tag_prefix to construct PrefixedTag.
            let monorepo_prefix = self.config.monorepo_tag_prefix();

            // monorepo.tag_prefix takes precedence over tag.tag_prefix for
            // PrefixedTag / PrefixedPreviousTag / PrefixedSummary behavior.
            // When monorepo is configured, info.tag and info.summary already
            // contain the prefix from git, so we strip for the base vars and
            // use the raw values for the Prefixed variants.
            if let Some(prefix) = monorepo_prefix {
                // Monorepo mode: the tag in git_info is the FULL prefixed tag.
                // PrefixedTag = full tag (already has prefix).
                self.template_vars.set("PrefixedTag", &info.tag);

                // Tag = prefix stripped. Override the Tag we set above.
                let stripped_tag = crate::git::strip_monorepo_prefix(&info.tag, prefix);
                self.template_vars.set("Tag", stripped_tag);

                // Version: derived from the parsed SemVer struct (same source as
                // the non-monorepo path and the build stage's per-crate
                // re-scoping) so all three stay byte-identical. `info.semver`
                // was parsed from the full prefixed tag, so it already excludes
                // the monorepo prefix — no separate string-strip needed.
                //
                // For a non-semver tag under `--skip=validate`, info.semver is
                // the skip-validate fallback, so this yields "0.0.0" rather than
                // the old raw prefix-stripped string.
                let version = info.semver.version_string();
                self.template_vars.set("Version", &version);

                // PrefixedPreviousTag = full previous tag (already has prefix).
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                self.template_vars.set("PrefixedPreviousTag", prev_tag);

                // PreviousTag = prefix stripped, consistent with Tag being stripped.
                let stripped_prev = crate::git::strip_monorepo_prefix(prev_tag, prefix);
                self.template_vars.set("PreviousTag", stripped_prev);

                // PrefixedSummary: info.summary from `git describe` already
                // includes the monorepo prefix (e.g. "subproject1/v1.2.3-0-gabc123d"),
                // so use it as-is for the prefixed variant.
                self.template_vars.set("PrefixedSummary", &info.summary);
                // Summary: strip the monorepo prefix for the base variant.
                let stripped_summary = crate::git::strip_monorepo_prefix(&info.summary, prefix);
                self.template_vars.set("Summary", stripped_summary);
            } else {
                // Non-monorepo: prepend tag.tag_prefix to construct PrefixedTag.
                let tag_prefix = self
                    .config
                    .tag
                    .as_ref()
                    .and_then(|t| t.tag_prefix.as_deref())
                    .unwrap_or("");
                self.template_vars
                    .set("PrefixedTag", &format!("{}{}", tag_prefix, info.tag));
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                let prefixed_prev = if prev_tag.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", tag_prefix, prev_tag)
                };
                self.template_vars
                    .set("PrefixedPreviousTag", &prefixed_prev);
                self.template_vars.set(
                    "PrefixedSummary",
                    &format!("{}{}", tag_prefix, info.summary),
                );
            }
        }

        // `NightlyBuild`: stateless per-base-version build counter derived
        // from `git rev-list --count <last-tag>..HEAD`. Resets automatically
        // when a new version tag lands (no state anodizer persists). Set
        // unconditionally (it is just a count), but intended for nightly /
        // snapshot `version_template`s such as
        // `"{{ .Base }}-nightly.{{ .NightlyBuild }}+{{ .ShortCommit }}"`.
        // Defaults to "0" outside a git repo (synthetic snapshot/scratch
        // builds) and on any git error so templates never fail to render.
        //
        // The monorepo prefix constrains the last-tag lookup to the active
        // crate's tags so per-crate workspace runs count since the right
        // tag (not the nearest tag from another subproject).
        let nightly_build = if self.git_info.is_some() {
            let root = self
                .options
                .project_root
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            let monorepo_prefix = self.config.monorepo_tag_prefix();
            crate::git::count_commits_since_last_tag_in(&root, monorepo_prefix).unwrap_or(0)
        } else {
            0
        };
        self.template_vars
            .set_structured("NightlyBuild", serde_json::Value::from(nightly_build));

        // Mode flags are injected as real bools (not "true"/"false" strings)
        // so `not IsSnapshot` / `IsSnapshot == false` / bare `{% if … %}`
        // forms all evaluate correctly; `{{ IsSnapshot }}` interpolation
        // still renders "true"/"false".
        self.template_vars
            .set_bool("IsSnapshot", self.options.snapshot);
        self.template_vars
            .set_bool("IsNightly", self.options.nightly);
        // Surfaced to user `if_condition:` templates so stages can
        // selectively run inside the determinism harness even when
        // `not IsSnapshot` would otherwise skip them.
        self.template_vars.set_bool(
            "IsHarness",
            self.env_var("ANODIZER_IN_DETERMINISM_HARNESS").is_some(),
        );
        // Wire IsDraft from `release.draft`.
        let is_draft = self
            .config
            .release
            .as_ref()
            .and_then(|r| r.draft)
            .unwrap_or(false);
        self.template_vars.set_bool("IsDraft", is_draft);
        self.template_vars
            .set_bool("IsSingleTarget", self.options.single_target.is_some());

        // Pro addition: IsRelease — true if this is a regular release (not snapshot, not nightly).
        let is_release = !self.options.snapshot && !self.options.nightly;
        self.template_vars.set_bool("IsRelease", is_release);

        // Pro addition: IsMerging — true if running with --merge flag.
        self.template_vars.set_bool("IsMerging", self.options.merge);
    }

    /// Populate time-related template variables.
    ///
    /// Sets:
    /// - `Date` — UTC time as RFC 3339
    /// - `Timestamp` — unix timestamp as string
    /// - `Now` — UTC time as RFC 3339
    /// - `Year` — four-digit year (e.g. "2026")
    /// - `Month` — zero-padded month (e.g. "03")
    /// - `Day` — zero-padded day (e.g. "30")
    /// - `Hour` — zero-padded hour (e.g. "14")
    /// - `Minute` — zero-padded minute (e.g. "05")
    ///
    /// Time source resolution (first match wins):
    ///
    /// 1. `SOURCE_DATE_EPOCH` env var — the standard reproducibility contract
    ///    (set by the determinism harness on every child release subprocess,
    ///    and the conventional way external CI / packagers signal a fixed
    ///    epoch). This is load-bearing for byte-stability of `metadata.json`
    ///    (which embeds `Date`) and any user template that consumes `Date` /
    ///    `Timestamp` / `Now`. Without this branch, two from-clean runs of
    ///    the same commit emit metadata.json files that differ in the `date`
    ///    field, defeating release-asset idempotency.
    /// 2. `chrono::Utc::now()` — wall-clock fallback. The
    ///    legacy semantics for runs without SDE wired in. Note that the
    ///    template docs explicitly call `.Now` "not deterministic"
    ///    — under SDE-aware reproducible builds we deviate from that
    ///    behavior intentionally.
    pub fn populate_time_vars(&mut self) {
        // Resolution order (SDE first, else wall-clock) is centralized in
        // `crate::sde::resolve_now_with_env` so any caller —
        // `populate_time_vars`, Tera built-ins, stage-srpm's `%changelog`
        // date, nightly `date_str` — sees identical "now" semantics.
        // Routes through the injected `env_source` so tests can inject
        // SOURCE_DATE_EPOCH via TestContextBuilder::env() without
        // mutating the process env.
        let now = crate::sde::resolve_now_with_env(self.env_source());
        self.template_vars.set("Date", &now.to_rfc3339());
        self.template_vars
            .set("Timestamp", &now.timestamp().to_string());
        self.template_vars.set("Now", &now.to_rfc3339());
        self.template_vars
            .set("Year", &now.format("%Y").to_string());
        self.template_vars
            .set("Month", &now.format("%m").to_string());
        self.template_vars.set("Day", &now.format("%d").to_string());
        self.template_vars
            .set("Hour", &now.format("%H").to_string());
        self.template_vars
            .set("Minute", &now.format("%M").to_string());
    }

    /// Populate runtime environment variables.
    ///
    /// Sets:
    /// - `RuntimeGoos` — host OS in Go-compatible naming (e.g. "linux", "darwin", "windows")
    /// - `RuntimeGoarch` — host architecture in Go-compatible naming (e.g. "amd64", "arm64")
    /// - `Runtime_Goos` / `Runtime_Goarch` — nested aliases
    /// - `RustcVersion` — host rustc release version (e.g. "1.96.0"), or "" when
    ///   rustc is unavailable
    pub fn populate_runtime_vars(&mut self) {
        let goos = map_os_to_goos(std::env::consts::OS);
        let goarch = map_arch_to_goarch(std::env::consts::ARCH);
        self.template_vars.set("RuntimeGoos", goos);
        self.template_vars.set("RuntimeGoarch", goarch);
        // Runtime.Goos / Runtime.Goarch — after preprocessing
        // the dot becomes an underscore-separated flat key. We expose both forms.
        self.template_vars.set("Runtime_Goos", goos);
        self.template_vars.set("Runtime_Goarch", goarch);
        // RustcVersion is a host-environment fact like OS/arch, so it is set in
        // the same call — keeping it a separate populate step risks a call-site
        // forgetting to invoke the sibling.
        self.populate_rustc_vars();
    }

    /// Populate the `RustcVersion` built-in template variable.
    ///
    /// Probes `rustc -vV` and extracts the `release:` line (e.g. `"1.96.0"`).
    /// Sets `RustcVersion` to the extracted string, or to `""` when rustc is
    /// unavailable or the line is absent — templates that reference
    /// `{{ .RustcVersion }}` degrade to an empty value rather than erroring.
    fn populate_rustc_vars(&mut self) {
        let ver = crate::partial::detect_rustc_version().unwrap_or_default();
        self.template_vars.set("RustcVersion", &ver);
    }

    /// Populate the `ReleaseNotes` template variable from stored changelogs.
    ///
    /// Should be called after the changelog stage has run and populated
    /// `self.stage_outputs.changelogs`. Uses the first crate (by crate
    /// universe order — top-level `crates:` then every `workspaces[].crates`
    /// entry) whose changelog is present, or an empty string if no
    /// changelogs exist. Universe order is deterministic, unlike HashMap
    /// iteration order.
    pub fn populate_release_notes_var(&mut self) {
        // Look up changelogs in universe order for determinism. The universe
        // walk (not `config.crates`) is what lets a pure-`workspaces:` config
        // resolve a non-empty `ReleaseNotes` — its crates carry the
        // changelogs but never appear in the top-level list.
        let notes = self
            .config
            .crate_universe()
            .into_iter()
            .find_map(|c| self.stage_outputs.changelogs.get(&c.name))
            .cloned()
            .unwrap_or_default();
        self.template_vars.set("ReleaseNotes", &notes);
    }

    /// Refresh the `Artifacts` structured template variable from the current
    /// artifact registry. Should be called before rendering release body and
    /// announce templates so they can iterate over all artifacts.
    ///
    /// Each artifact is serialized as a map with keys: `name`, `path`, `target`,
    /// `kind`, `crate_name`, and `metadata`.
    ///
    /// **Known metadata keys** (populated by individual stages):
    /// - `format` — archive format (e.g. `"tar.gz"`, `"zip"`), set by archive stage
    /// - `extra_file` — `"true"` when artifact is an extra file, set by checksum stage
    /// - `extra_name_template` — name template override for extra files, set by checksum stage
    /// - `digest` — docker image digest (e.g. `sha256:abc123...`), set by docker stage
    /// - `id` — artifact ID from config, set by docker and build stages
    /// - `binary` — binary name, set by build stage
    pub fn refresh_artifacts_var(&mut self) {
        // CSV metadata keys we expose as JSON arrays for template iteration.
        // Storage remains HashMap<String,String> (flat); only the
        // template-exposed view is expanded. The
        // ExtraBinaries / ExtraFiles list semantics.
        const CSV_LIST_KEYS: &[&str] = &["extra_binaries", "extra_files"];
        // JSON-encoded list metadata keys: stored as a JSON-array string in
        // `HashMap<String,String>`, exposed as a real array on the template
        // side so `{% for p in .Artifacts[0].metadata.Platforms %}` works.
        // `Platforms` is the platform-list slice on
        // `DockerImageV2` artifacts.
        const JSON_LIST_KEYS: &[&str] = &["Platforms"];

        let artifacts_value: Vec<serde_json::Value> = self
            .artifacts
            .all()
            .iter()
            .map(|a| {
                // Rebuild metadata map converting known CSV keys into arrays.
                let mut metadata_map = serde_json::Map::with_capacity(a.metadata.len());
                for (k, v) in &a.metadata {
                    if CSV_LIST_KEYS.contains(&k.as_str()) {
                        let items: Vec<serde_json::Value> = if v.is_empty() {
                            Vec::new()
                        } else {
                            v.split(',')
                                .map(|s| serde_json::Value::String(s.to_string()))
                                .collect()
                        };
                        metadata_map.insert(k.clone(), serde_json::Value::Array(items));
                    } else if JSON_LIST_KEYS.contains(&k.as_str()) {
                        // Decode JSON-array string into a real Value::Array;
                        // a malformed value falls back to the raw string so
                        // custom publishers can still inspect it.
                        let parsed = serde_json::from_str::<serde_json::Value>(v)
                            .unwrap_or_else(|_| serde_json::Value::String(v.clone()));
                        metadata_map.insert(k.clone(), parsed);
                    } else {
                        metadata_map.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                }
                serde_json::json!({
                    "name": a.name,
                    "path": a.path.to_string_lossy(),
                    "target": a.target.as_deref().unwrap_or(""),
                    "kind": a.kind.as_str(),
                    "crate_name": a.crate_name,
                    "metadata": serde_json::Value::Object(metadata_map),
                })
            })
            .collect();
        self.template_vars
            .set_structured("Artifacts", serde_json::Value::Array(artifacts_value));
    }

    /// Populate the `Metadata` structured template variable from config.metadata.
    ///
    /// Exposes the project metadata block as a nested map with PascalCase keys
    /// the `.Metadata.*` namespace:
    /// `Description`, `Homepage`, `Documentation`, `License`, `Repository`,
    /// `Maintainers`, `ModTimestamp`, `FullDescription` (resolved),
    /// `CommitAuthor.{Name,Email}`.
    /// Missing fields default to empty strings / empty arrays.
    ///
    /// `full_description` supports `Inline`, `FromFile` (template-rendered
    /// path, read from disk), and `FromUrl` (template-rendered URL +
    /// headers, fetched through [`crate::content_source::resolve`] which
    /// applies retries, body caps, and CR/LF header-injection guards).
    pub fn populate_metadata_var(&mut self) -> anyhow::Result<()> {
        // Clone the small scalar fields so we don't hold a borrow on self.config
        // across the render_template calls below.
        let (
            description,
            homepage,
            documentation,
            license,
            repository,
            maintainers,
            mod_timestamp,
            full_desc_src,
            commit_author,
        ) = {
            let meta = self.config.metadata.as_ref();
            // Description / homepage / documentation / license resolve through
            // the project-level fallback: top-level `metadata.*` wins, else the
            // primary crate's `Cargo.toml`-derived value. This keeps
            // `{{ Metadata.* }}` single-sourced with the per-publisher
            // `meta_*_for` resolvers, so dropping a redundant `metadata.license`
            // (derivable from Cargo.toml) does not silently empty the var.
            let description = self
                .config
                .meta_description_project()
                .unwrap_or("")
                .to_string();
            let homepage = self
                .config
                .meta_homepage_project()
                .unwrap_or("")
                .to_string();
            let documentation = self
                .config
                .meta_documentation_project()
                .unwrap_or("")
                .to_string();
            let license = self.config.meta_license_project().unwrap_or("").to_string();
            let repository = self
                .config
                .meta_repository_project()
                .unwrap_or("")
                .to_string();
            let maintainers: Vec<String> = meta
                .and_then(|m| m.maintainers.as_ref())
                .cloned()
                .unwrap_or_default();
            let mod_timestamp = meta
                .and_then(|m| m.mod_timestamp.as_deref())
                .unwrap_or("")
                .to_string();
            let full_desc_src = meta.and_then(|m| m.full_description.clone());
            let commit_author = meta.and_then(|m| m.commit_author.clone());
            (
                description,
                homepage,
                documentation,
                license,
                repository,
                maintainers,
                mod_timestamp,
                full_desc_src,
                commit_author,
            )
        };

        // Resolve full_description through the shared ContentSource resolver
        // so Inline, FromFile (template-rendered path), and FromUrl
        // (template-rendered URL + headers, retried HTTP fetch with
        // body cap and CR/LF guard) all behave the same as the release
        // header/footer fields.
        let full_description = match full_desc_src {
            None => String::new(),
            Some(src) => crate::content_source::resolve(
                &src,
                "metadata.full_description",
                self,
                &self.logger("metadata"),
            )?,
        };

        let commit_author_map = serde_json::json!({
            "Name": commit_author.as_ref().and_then(|c| c.name.clone()).unwrap_or_default(),
            "Email": commit_author.as_ref().and_then(|c| c.email.clone()).unwrap_or_default(),
        });

        let meta_map = serde_json::json!({
            "Description": description,
            "Homepage": homepage,
            "Documentation": documentation,
            "License": license,
            "Repository": repository,
            "Maintainers": maintainers,
            "ModTimestamp": mod_timestamp,
            "FullDescription": full_description,
            "CommitAuthor": commit_author_map,
        });
        self.template_vars.set_structured("Metadata", meta_map);
        Ok(())
    }
}

/// Map Rust's `std::env::consts::OS` to Go-compatible GOOS naming.
/// Templates expect Go runtime names (e.g. "darwin" not "macos").
pub fn map_os_to_goos(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other, // linux, windows, freebsd, etc. already match
    }
}

/// Map Rust's `std::env::consts::ARCH` to Go-compatible GOARCH naming.
/// Templates expect Go runtime names (e.g. "amd64" not "x86_64").
///
/// Delegates to the shared [`crate::target::rust_arch_to_goarch`] table so a
/// host-derived `{{ .Runtime.Goarch }}` can never disagree with the
/// triple-derived arch tokens in asset names. `ARCH` doesn't encode
/// endianness, so the host's own compile-time endianness disambiguates
/// `powerpc64`/`mips64`. Tokens outside the table (`arm` — GOARCH really is
/// "arm" — plus exotics) pass through unchanged.
pub fn map_arch_to_goarch(arch: &str) -> &str {
    crate::target::rust_arch_to_goarch(arch, cfg!(target_endian = "little")).unwrap_or(arch)
}

/// Set the full version-derived template var block (`Tag`, `Version`,
/// `RawVersion`, `Base`, `Major`, `Minor`, `Patch`, `Prerelease`,
/// `BuildMetadata`) from a parsed `semver` and the release `tag`. The single
/// source of truth for this block, shared by [`Context::populate_git_vars`] (the
/// context's own git version) and [`Context::render_template_for_version`] (a
/// promotion's target version) so the two can never drift.
fn set_version_vars(vars: &mut TemplateVars, semver: &crate::git::SemVer, tag: &str) {
    // RawVersion: major.minor.patch only, no prerelease / build metadata.
    let raw_version = semver.raw_version_string();
    // Version: clean semver derived from the parsed struct (handles every
    // tag_template prefix, e.g. monorepo `core-v0.3.2`).
    let version = semver.version_string();

    vars.set("Tag", tag);
    vars.set("Version", &version);
    vars.set("RawVersion", &raw_version);
    // `Base`: the numeric base semver, captured before snapshot/nightly version
    // templating overwrites `Version`, for schemes like
    // `"{{ .Base }}-nightly.{{ .NightlyBuild }}+{{ .ShortCommit }}"`.
    vars.set("Base", &raw_version);
    vars.set("Major", &semver.major.to_string());
    vars.set("Minor", &semver.minor.to_string());
    vars.set("Patch", &semver.patch.to_string());
    vars.set("Prerelease", semver.prerelease.as_deref().unwrap_or(""));
    vars.set(
        "BuildMetadata",
        semver.build_metadata.as_deref().unwrap_or(""),
    );
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::{GitInfo, SemVer};
    use crate::test_helpers::env::env_mutex;
    use std::collections::BTreeSet;

    /// A `StageLogger` built via `Context::logger` before a secret is minted
    /// into `env_source` (e.g. crates.io Trusted Publishing overlaying
    /// `CARGO_REGISTRY_TOKEN` mid-run via `begin_cargo_trusted_publishing`)
    /// must still redact that secret: the logger holds a live handle to the
    /// context's redaction table, not a frozen construction-time snapshot.
    #[test]
    fn stage_logger_redacts_secret_minted_after_construction() {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.set_env_source(crate::MapEnvSource::new());
        let log = ctx.logger("cargo");

        ctx.set_env_source(crate::MapEnvSource::new().with("SOMETHING_TOKEN", "supersecret123"));

        let redacted = log.redact("publish failed: token=supersecret123 rejected");
        assert!(
            !redacted.contains("supersecret123"),
            "logger built before the mint must still redact a secret added afterward: {redacted}"
        );
        assert!(
            redacted.contains("$SOMETHING_TOKEN"),
            "redacted output should substitute the env-var name: {redacted}"
        );
    }

    /// `env_for_redact` must honor an injected/sealed `env_source` instead of
    /// unconditionally reading `std::env::vars()` — otherwise a hermetic test
    /// that seals its env can still leak an unrelated real ambient
    /// secret-suffixed var into a `StageLogger`'s redaction table (silently
    /// masking substrings of literal test fixture text that happen to
    /// collide with the ambient value).
    #[test]
    fn env_for_redact_honors_injected_env_source_not_real_process_env() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let key = "ANODIZER_T3_ENV_REDACT_FIXTURE_TOKEN";
        // SAFETY: serialised by env_mutex; cleaned up before guard drop.
        // env-ok: contract test for env_for_redact source routing; unique key.
        unsafe { std::env::set_var(key, "should-not-leak") };

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.set_env_source(crate::MapEnvSource::new());
        let log = ctx.logger("test");
        let redacted = log.redact("value=should-not-leak");

        // SAFETY: serialised by env_mutex.
        // env-ok: contract test for env_for_redact source routing; unique key.
        unsafe { std::env::remove_var(key) };

        assert_eq!(
            redacted, "value=should-not-leak",
            "a sealed env_source must not let the real ambient var mask this literal value"
        );
    }

    /// `VALID_RELEASE_SKIPS` MUST recognize every publisher token. Driven off
    /// [`PublisherKind::iter`] so a newly added publisher that is not folded
    /// into the `--skip` vocabulary trips immediately. Pins the nine tokens
    /// that had silently dropped out of the former hand-maintained literal.
    #[test]
    fn valid_release_skips_is_superset_of_every_publisher_token() {
        let skips: BTreeSet<&str> = VALID_RELEASE_SKIPS.iter().copied().collect();
        for k in PublisherKind::iter() {
            assert!(
                skips.contains(k.token()),
                "VALID_RELEASE_SKIPS missing publisher token `{}` — `--skip={}` would be \
                 silently rejected",
                k.token(),
                k.token(),
            );
        }
        for previously_missing in [
            "npm",
            "gemfury",
            "cloudsmith",
            "artifactory",
            "uploads",
            "dockerhub",
            "mcp",
            "schemastore",
            "upstream-aur",
        ] {
            assert!(
                skips.contains(previously_missing),
                "publisher token `{previously_missing}` (one of the nine that had dropped out \
                 of the old literal) is still not a recognized --skip value"
            );
        }
    }

    /// The non-publisher half of the vocabulary must stay disjoint from the
    /// publisher tokens, so the union has a single, unambiguous owner per
    /// token. (`snapcraft`/`snapcraft-publish` and `release`/`github-release`
    /// are the deliberately-distinct stage-vs-publisher pairs.)
    #[test]
    fn non_publisher_release_skips_disjoint_from_publisher_tokens() {
        let publisher_tokens: BTreeSet<&str> =
            PublisherKind::iter().map(PublisherKind::token).collect();
        for stage in NON_PUBLISHER_RELEASE_SKIPS {
            assert!(
                !publisher_tokens.contains(stage),
                "`{stage}` is listed in NON_PUBLISHER_RELEASE_SKIPS but is also a publisher token"
            );
        }
    }

    /// By construction: the token set `anodizer vocabulary` emits equals
    /// [`VALID_RELEASE_SKIPS`] exactly — same members, no duplicates. Both are
    /// derived from the same SSOT ([`NON_PUBLISHER_RELEASE_SKIPS`] ∪
    /// [`PublisherKind::iter`]), so a newly added publisher or stage token
    /// flows into both at once; this pins that they can never diverge.
    #[test]
    fn release_skip_vocabulary_token_set_equals_valid_release_skips() {
        let vocab = release_skip_vocabulary();
        let emitted: BTreeSet<&str> = vocab.iter().map(|t| t.token).collect();
        let valid: BTreeSet<&str> = VALID_RELEASE_SKIPS.iter().copied().collect();
        assert_eq!(
            emitted, valid,
            "`anodizer vocabulary` token set drifted from VALID_RELEASE_SKIPS"
        );
        assert_eq!(
            vocab.len(),
            emitted.len(),
            "release_skip_vocabulary emitted a duplicate token"
        );
    }

    /// Each vocabulary entry classifies itself consistently with the SSOT:
    /// publisher entries carry [`PublisherKind::is_publish_stage`]; the
    /// non-publisher stage tokens are never marked as publishers or publish
    /// stages.
    #[test]
    fn release_skip_vocabulary_flags_match_publisher_kind() {
        let vocab = release_skip_vocabulary();
        for entry in &vocab {
            if entry.is_publisher {
                let kind = PublisherKind::iter()
                    .find(|k| k.token() == entry.token)
                    .unwrap_or_else(|| panic!("publisher entry `{}` has no kind", entry.token));
                assert_eq!(
                    entry.is_publish_stage,
                    kind.is_publish_stage(),
                    "is_publish_stage for `{}` drifted from PublisherKind",
                    entry.token
                );
            } else {
                assert!(
                    !entry.is_publish_stage,
                    "non-publisher token `{}` must not be a publish stage",
                    entry.token
                );
                assert!(
                    NON_PUBLISHER_RELEASE_SKIPS.contains(&entry.token),
                    "non-publisher entry `{}` is not in NON_PUBLISHER_RELEASE_SKIPS",
                    entry.token
                );
            }
        }
    }

    fn make_git_info(dirty: bool, prerelease: Option<&str>) -> GitInfo {
        let tag = match prerelease {
            Some(pre) => format!("v1.2.3-{pre}"),
            None => "v1.2.3".to_string(),
        };
        GitInfo {
            tag,
            commit: "abc123def456abc123def456abc123def456abc1".to_string(),
            short_commit: "abc123d".to_string(),
            branch: "main".to_string(),
            dirty,
            semver: SemVer {
                major: 1,
                minor: 2,
                patch: 3,
                prerelease: prerelease.map(|s| s.to_string()),
                build_metadata: None,
            },
            commit_date: "2026-03-25T10:30:00+00:00".to_string(),
            commit_timestamp: "1774463400".to_string(),
            previous_tag: Some("v1.2.2".to_string()),
            remote_url: "https://github.com/test/repo.git".to_string(),
            summary: "v1.2.3-0-gabc123d".to_string(),
            tag_subject: "Release v1.2.3".to_string(),
            tag_contents: "Release v1.2.3\n\nFull release notes here.".to_string(),
            tag_body: "Full release notes here.".to_string(),
            first_commit: None,
        }
    }

    #[test]
    fn test_context_template_vars() {
        let mut config = Config::default();
        config.project_name = "test-project".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"test-project".to_string())
        );
    }

    #[test]
    fn validate_skip_values_hint_dedups_overlapping_vocabulary() {
        // The release skip vocabulary is `VALID_RELEASE_SKIPS ++ publisher
        // names`, which legitimately overlap. A bad token must surface a hint
        // listing each valid option exactly ONCE, in first-seen order — not the
        // doubled list a raw `valid.join(", ")` produces.
        let valid = ["homebrew", "cargo", "npm", "homebrew", "cargo", "uploads"];
        let err = validate_skip_values(&["bogus".to_string()], &valid).unwrap_err();
        let opts = err
            .split("Valid options: ")
            .nth(1)
            .expect("hint must carry a Valid options list");
        assert_eq!(
            opts, "homebrew, cargo, npm, uploads",
            "valid options must be de-duplicated in first-seen order"
        );
    }

    #[test]
    fn validate_skip_values_dedups_repeated_invalid_tokens() {
        // The token must not be a substring of any valid option, or `matches`
        // would count the valid-options hint too (`uploads` contains `upload`).
        let err = validate_skip_values(
            &["bogusxyz".to_string(), "bogusxyz".to_string()],
            &VALID_RELEASE_SKIPS,
        )
        .unwrap_err();
        assert_eq!(
            err.matches("bogusxyz").count(),
            1,
            "a repeated invalid token must be reported once: {err}"
        );
    }

    #[test]
    fn test_context_should_skip() {
        let config = Config::default();
        let opts = ContextOptions {
            skip_stages: vec!["publish".to_string(), "announce".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(config, opts);
        assert!(ctx.should_skip("publish"));
        assert!(ctx.should_skip("announce"));
        assert!(!ctx.should_skip("build"));
    }

    #[test]
    fn publisher_deselected_empty_selectors_runs_everything() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!ctx.publisher_deselected("npm"));
        assert!(!ctx.publisher_deselected("cargo"));
        assert!(!ctx.publisher_deselected("anything"));
    }

    #[test]
    fn publisher_deselected_skip_denylists() {
        let opts = ContextOptions {
            skip_stages: vec!["npm".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), opts);
        assert!(ctx.publisher_deselected("npm"));
        assert!(!ctx.publisher_deselected("cargo"));
    }

    #[test]
    fn publisher_deselected_allowlist_excludes_unlisted() {
        let opts = ContextOptions {
            publisher_allowlist: vec!["cargo".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), opts);
        assert!(!ctx.publisher_deselected("cargo"));
        assert!(ctx.publisher_deselected("npm"));
    }

    #[test]
    fn publisher_deselected_skip_wins_over_allowlist() {
        let opts = ContextOptions {
            skip_stages: vec!["cargo".to_string()],
            publisher_allowlist: vec!["cargo".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), opts);
        assert!(ctx.publisher_deselected("cargo"));
    }

    #[test]
    fn any_publisher_selected_matches_deselection_dual() {
        let opts = ContextOptions {
            publisher_allowlist: vec!["cargo".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(Config::default(), opts);
        assert!(ctx.any_publisher_selected(&["npm", "cargo"]));
        assert!(!ctx.any_publisher_selected(&["npm", "blob"]));
        assert!(!ctx.any_publisher_selected(&[]));
    }

    #[test]
    fn test_context_render_template() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        let result = ctx.render_template("{{ .ProjectName }}-release").unwrap();
        assert_eq!(result, "myapp-release");
    }

    #[test]
    fn test_populate_git_vars_sets_all_expected_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("Major"), Some(&"1".to_string()));
        assert_eq!(v.get("Minor"), Some(&"2".to_string()));
        assert_eq!(v.get("Patch"), Some(&"3".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"".to_string()));
        assert_eq!(
            v.get("FullCommit"),
            Some(&"abc123def456abc123def456abc123def456abc1".to_string())
        );
        assert_eq!(v.get("ShortCommit"), Some(&"abc123d".to_string()));
        assert_eq!(v.get("Branch"), Some(&"main".to_string()));
        assert_eq!(
            v.get("CommitDate"),
            Some(&"2026-03-25T10:30:00+00:00".to_string())
        );
        assert_eq!(v.get("CommitTimestamp"), Some(&"1774463400".to_string()));
        assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
        // Base mirrors the numeric base semver, set before any
        // snapshot/nightly version templating overwrites Version.
        assert_eq!(v.get("Base"), Some(&"1.2.3".to_string()));
    }

    #[test]
    fn test_nightly_build_defaults_to_zero_without_git_info() {
        // No git_info (synthetic snapshot/scratch build): NightlyBuild must
        // render as "0" so version_templates referencing it never fail.
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = None;
        ctx.populate_git_vars();
        assert_eq!(
            ctx.template_vars().get_structured("NightlyBuild"),
            Some(&serde_json::Value::from(0u64))
        );
    }

    #[test]
    fn test_commit_is_alias_for_full_commit() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Commit"), v.get("FullCommit"));
    }

    #[test]
    fn test_populate_git_vars_prerelease() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, Some("rc.1")));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Version"), Some(&"1.2.3-rc.1".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"rc.1".to_string()));
    }

    #[test]
    fn test_build_metadata_template_var() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "v1.2.3+build.42".to_string();
        info.semver.build_metadata = Some("build.42".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("BuildMetadata"), Some(&"build.42".to_string()));
        // Version should include build metadata (strip v prefix only)
        assert_eq!(v.get("Version"), Some(&"1.2.3+build.42".to_string()));
    }

    #[test]
    fn test_build_metadata_empty_when_none() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("BuildMetadata"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_git_vars_monorepo_prefixed_tag() {
        // Workspace tags like "core-v0.3.2" should produce Version="0.3.2",
        // not "core-v0.3.2" (which breaks RPM Version fields and templates).
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "core-v0.3.2".to_string();
        info.semver = SemVer {
            major: 0,
            minor: 3,
            patch: 2,
            prerelease: None,
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"core-v0.3.2".to_string()));
        assert_eq!(v.get("Version"), Some(&"0.3.2".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"0.3.2".to_string()));
        assert_eq!(v.get("Major"), Some(&"0".to_string()));
        assert_eq!(v.get("Minor"), Some(&"3".to_string()));
        assert_eq!(v.get("Patch"), Some(&"2".to_string()));
    }

    #[test]
    fn test_populate_git_vars_monorepo_prefixed_tag_with_prerelease() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "operator-v1.0.0-rc.1".to_string();
        info.semver = SemVer {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: Some("rc.1".to_string()),
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"operator-v1.0.0-rc.1".to_string()));
        assert_eq!(v.get("Version"), Some(&"1.0.0-rc.1".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.0.0".to_string()));
    }

    #[test]
    fn test_git_tree_state_clean() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(
            v.get_structured("IsGitDirty"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(v.get("GitTreeState"), Some(&"clean".to_string()));
    }

    #[test]
    fn test_git_tree_state_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(
            v.get_structured("IsGitDirty"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(v.get("GitTreeState"), Some(&"dirty".to_string()));
    }

    #[test]
    fn test_is_snapshot_reflects_context_options() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsSnapshot"),
            Some(&serde_json::Value::Bool(true))
        );

        // Non-snapshot
        let config2 = Config::default();
        let opts2 = ContextOptions {
            snapshot: false,
            ..Default::default()
        };
        let mut ctx2 = Context::new(config2, opts2);
        ctx2.git_info = Some(make_git_info(false, None));
        ctx2.populate_git_vars();

        assert_eq!(
            ctx2.template_vars().get_structured("IsSnapshot"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_is_draft_defaults_to_false() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsDraft"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_previous_tag_empty_when_none() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PreviousTag"),
            Some(&"".to_string())
        );
    }

    /// Regression: `populate_time_vars` MUST derive `Date` / `Timestamp` /
    /// `Now` (and the calendar fields) from `SOURCE_DATE_EPOCH` when the
    /// env var is set — the standard reproducible-build contract the
    /// determinism harness depends on. Two from-clean runs of the same
    /// commit otherwise emit `dist/metadata.json` files that differ in
    /// the embedded `date` field, drifting `metadata.json` AND its
    /// `.sha256` sidecar across runs. CI run 25975073213 surfaced this
    /// drift on every platform shard before the fix landed.
    #[test]
    fn populate_time_vars_uses_source_date_epoch_when_set() {
        // 1_715_000_000 = 2024-05-06T12:53:20+00:00 — picked to be safely
        // earlier than wall-clock so a wall-clock-derived assertion would
        // visibly fail.
        let env = crate::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(env);
        ctx.populate_time_vars();

        let v = ctx.template_vars();
        assert_eq!(
            v.get("Timestamp"),
            Some(&"1715000000".to_string()),
            "Timestamp must equal SOURCE_DATE_EPOCH seconds"
        );
        assert_eq!(
            v.get("Date"),
            Some(&"2024-05-06T12:53:20+00:00".to_string()),
            "Date must be RFC 3339 derived from SDE"
        );
        assert_eq!(v.get("Year"), Some(&"2024".to_string()));
        assert_eq!(v.get("Month"), Some(&"05".to_string()));
        assert_eq!(v.get("Day"), Some(&"06".to_string()));
    }

    #[test]
    fn test_populate_time_vars() {
        // Wall-clock fallback path: empty MapEnvSource has no
        // SOURCE_DATE_EPOCH, so we exercise the chrono::Utc::now() branch.
        let env = crate::MapEnvSource::new();
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(env);
        ctx.populate_time_vars();

        let v = ctx.template_vars();

        // Date should be RFC 3339 format (e.g. 2026-03-30T12:00:00+00:00)
        let date = v
            .get("Date")
            .unwrap_or_else(|| panic!("Date should be set"));
        assert!(
            date.contains('T') && date.len() > 10,
            "Date should be RFC 3339, got: {date}"
        );

        // Timestamp should be numeric
        let ts = v
            .get("Timestamp")
            .unwrap_or_else(|| panic!("Timestamp should be set"));
        assert!(
            ts.parse::<i64>().is_ok(),
            "Timestamp should be a numeric string, got: {ts}"
        );

        // Now should be ISO 8601
        let now = v.get("Now").unwrap_or_else(|| panic!("Now should be set"));
        assert!(now.contains('T'), "Now should be ISO 8601, got: {now}");
    }

    #[test]
    fn test_env_vars_accessible_in_templates() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set_env("MY_VAR", "hello-world");
        ctx.template_vars_mut().set_env("DEPLOY_ENV", "staging");

        let result = ctx
            .render_template("{{ .Env.MY_VAR }}-{{ .Env.DEPLOY_ENV }}")
            .unwrap();
        assert_eq!(result, "hello-world-staging");
    }

    #[test]
    fn test_populate_git_vars_without_git_info_still_sets_snapshot() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // Don't set git_info — populate_git_vars should still set IsSnapshot/IsDraft
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsSnapshot"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            ctx.template_vars().get_structured("IsDraft"),
            Some(&serde_json::Value::Bool(false))
        );
        // Git-specific vars should NOT be set
        assert_eq!(ctx.template_vars().get("Tag"), None);
    }

    #[test]
    fn test_is_nightly_set_when_nightly_mode_active() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsNightly"),
            Some(&serde_json::Value::Bool(true)),
            "IsNightly should be 'true' when nightly mode is active"
        );
        assert!(ctx.is_nightly(), "is_nightly() should return true");
    }

    #[test]
    fn test_is_nightly_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsNightly"),
            Some(&serde_json::Value::Bool(false)),
            "IsNightly should default to 'false'"
        );
        assert!(
            !ctx.is_nightly(),
            "is_nightly() should return false by default"
        );
    }

    #[test]
    fn test_version_returns_populated_value() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(ctx.version(), "1.2.3");
    }

    #[test]
    fn test_version_returns_empty_when_not_set() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(ctx.version(), "");
    }

    #[test]
    fn test_is_nightly_without_git_info() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // No git_info set — populate_git_vars still sets IsNightly
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsNightly"),
            Some(&serde_json::Value::Bool(true)),
            "IsNightly should be set even without git info"
        );
    }

    #[test]
    fn test_is_git_clean_when_not_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsGitClean"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_is_git_clean_when_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsGitClean"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_git_url_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("GitURL"),
            Some(&"https://github.com/test/repo.git".to_string())
        );
    }

    #[test]
    fn test_summary_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("Summary"),
            Some(&"v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_tag_subject_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagSubject"),
            Some(&"Release v1.2.3".to_string())
        );
    }

    #[test]
    fn test_tag_contents_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagContents"),
            Some(&"Release v1.2.3\n\nFull release notes here.".to_string())
        );
    }

    #[test]
    fn test_tag_body_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagBody"),
            Some(&"Full release notes here.".to_string())
        );
    }

    #[test]
    fn test_is_single_target_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsSingleTarget"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_is_single_target_true_when_set() {
        let config = Config::default();
        let opts = ContextOptions {
            single_target: Some("x86_64-unknown-linux-gnu".to_string()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsSingleTarget"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_populate_runtime_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_runtime_vars();

        let v = ctx.template_vars();

        let goos = v
            .get("RuntimeGoos")
            .unwrap_or_else(|| panic!("RuntimeGoos should be set"));
        assert!(
            !goos.is_empty(),
            "RuntimeGoos should not be empty, got: {goos}"
        );
        // RuntimeGoos uses Go naming (e.g. "darwin" not "macos")
        assert_eq!(goos, map_os_to_goos(std::env::consts::OS));

        let goarch = v
            .get("RuntimeGoarch")
            .unwrap_or_else(|| panic!("RuntimeGoarch should be set"));
        assert!(
            !goarch.is_empty(),
            "RuntimeGoarch should not be empty, got: {goarch}"
        );
        // RuntimeGoarch uses Go naming (e.g. "amd64" not "x86_64")
        assert_eq!(goarch, map_arch_to_goarch(std::env::consts::ARCH));
    }

    #[test]
    fn test_map_arch_to_goarch_matches_shared_table() {
        // Host template vars and triple-derived asset tokens share one table:
        // loongarch64 must reach "loong64" (the former private copy passed it
        // through verbatim, so host renders never matched asset names) and the
        // endian-ambiguous hosts resolve by this build's endianness.
        assert_eq!(map_arch_to_goarch("x86_64"), "amd64");
        assert_eq!(map_arch_to_goarch("aarch64"), "arm64");
        assert_eq!(map_arch_to_goarch("x86"), "386");
        assert_eq!(map_arch_to_goarch("loongarch64"), "loong64");
        assert_eq!(map_arch_to_goarch("sparc64"), "sparc64");
        assert_eq!(
            map_arch_to_goarch("powerpc64"),
            crate::target::rust_arch_to_goarch("powerpc64", cfg!(target_endian = "little"))
                .unwrap()
        );
        // GOARCH for 32-bit ARM really is "arm" — passthrough, not a mapping gap.
        assert_eq!(map_arch_to_goarch("arm"), "arm");
    }

    #[test]
    fn test_populate_release_notes_var_with_changelogs() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "my-crate".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.stage_outputs
            .changelogs
            .insert("my-crate".to_string(), "## Changes\n- fix bug".to_string());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"## Changes\n- fix bug".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_empty_when_no_changelogs() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_deterministic_with_multiple_crates() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "crate-a".to_string(),
            ..Default::default()
        });
        config.crates.push(crate::config::CrateConfig {
            name: "crate-b".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.stage_outputs
            .changelogs
            .insert("crate-a".to_string(), "notes-a".to_string());
        ctx.stage_outputs
            .changelogs
            .insert("crate-b".to_string(), "notes-b".to_string());
        ctx.populate_release_notes_var();

        // Should always pick the first crate in config order, not arbitrary HashMap order
        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"notes-a".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_sees_workspace_only_crates() {
        // Pure-`workspaces:` config: the crates carrying the changelogs never
        // appear in the top-level `crates:` list, so the lookup must walk the
        // crate universe or `ReleaseNotes` renders empty.
        let config = Config {
            workspaces: Some(vec![crate::config::WorkspaceConfig {
                name: "grp".to_string(),
                crates: vec![crate::config::CrateConfig {
                    name: "member".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.stage_outputs
            .changelogs
            .insert("member".to_string(), "## member notes".to_string());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"## member notes".to_string()),
            "a workspace-only crate's changelog must populate ReleaseNotes"
        );
    }

    #[test]
    fn test_outputs_accessible_in_templates() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set_output("build_id", "abc123");
        ctx.template_vars_mut()
            .set_output("deploy_url", "https://example.com");

        let result = ctx
            .render_template("{{ .Outputs.build_id }}-{{ .Outputs.deploy_url }}")
            .unwrap();
        assert_eq!(result, "abc123-https://example.com");
    }

    #[test]
    fn test_artifact_ext_and_target_template_vars() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactName", "myapp.tar.gz");
        ctx.template_vars_mut().set("ArtifactExt", ".tar.gz");
        ctx.template_vars_mut()
            .set("Target", "x86_64-unknown-linux-gnu");

        let result = ctx
            .render_template("{{ .ArtifactExt }}_{{ .Target }}")
            .unwrap();
        assert_eq!(result, ".tar.gz_x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_checksums_template_var() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        let checksum_text = "abc123  myapp.tar.gz\ndef456  myapp.zip\n";
        ctx.template_vars_mut().set("Checksums", checksum_text);

        let result = ctx.render_template("{{ .Checksums }}").unwrap();
        assert_eq!(result, checksum_text);
    }

    // --- Pro template variable tests ---

    #[test]
    fn test_prefixed_tag_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedTag"),
            Some(&"api/v1.2.3".to_string())
        );
    }

    #[test]
    fn test_prefixed_tag_without_tag_prefix() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        // No tag_prefix configured — PrefixedTag should equal Tag
        assert_eq!(
            ctx.template_vars().get("PrefixedTag"),
            Some(&"v1.2.3".to_string())
        );
    }

    #[test]
    fn test_prefixed_previous_tag_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedPreviousTag"),
            Some(&"api/v1.2.2".to_string())
        );
    }

    #[test]
    fn test_prefixed_previous_tag_empty_when_no_previous() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        // When there is no previous tag, PrefixedPreviousTag should be empty
        // (not just the prefix).
        assert_eq!(
            ctx.template_vars().get("PrefixedPreviousTag"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_prefixed_summary_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedSummary"),
            Some(&"api/v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_is_release_true_for_normal_release() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsRelease"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_is_release_false_for_snapshot() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsRelease"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_is_release_false_for_nightly() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsRelease"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_is_merging_true_when_merge_flag_set() {
        let config = Config::default();
        let opts = ContextOptions {
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsMerging"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_is_merging_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsMerging"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_refresh_artifacts_var_empty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.refresh_artifacts_var();

        // Should render as an empty array
        let result = ctx
            .render_template("{% for a in Artifacts %}{{ a.name }}{% endfor %}")
            .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_refresh_artifacts_var_with_artifacts() {
        use crate::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        // Artifacts are created with empty `name` — ArtifactRegistry::add()
        // auto-derives the name from the path's filename component when name
        // is empty (see artifact.rs add() implementation).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.refresh_artifacts_var();

        // Iterate over artifacts and collect names
        let result = ctx
            .render_template("{% for a in Artifacts %}{{ a.name }},{% endfor %}")
            .unwrap();
        assert!(result.contains("myapp-1.0.0-linux-amd64.tar.gz"));
        assert!(result.contains("myapp"));

        // Check kind field
        let result_kinds = ctx
            .render_template("{% for a in Artifacts %}{{ a.kind }},{% endfor %}")
            .unwrap();
        assert!(result_kinds.contains("archive"));
        assert!(result_kinds.contains("binary"));
    }

    #[test]
    fn test_populate_metadata_var_with_mod_timestamp() {
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            mod_timestamp: Some("{{ .CommitTimestamp }}".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        // Metadata should be accessible as a nested map with PascalCase keys
        let result = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
        assert_eq!(result, "{{ .CommitTimestamp }}");
    }

    #[test]
    fn test_populate_metadata_var_empty_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        // Should render empty strings for missing fields (PascalCase keys)
        let result = ctx.render_template("{{ Metadata.Description }}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_populate_metadata_var_reads_from_config() {
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            description: Some("A test project".to_string()),
            homepage: Some("https://example.com".to_string()),
            documentation: Some("https://docs.example.com".to_string()),
            license: Some("MIT".to_string()),
            repository: Some("https://github.com/example/test".to_string()),
            maintainers: Some(vec!["Alice".to_string(), "Bob".to_string()]),
            mod_timestamp: Some("1234567890".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        let desc = ctx.render_template("{{ Metadata.Description }}").unwrap();
        assert_eq!(desc, "A test project");

        let home = ctx.render_template("{{ Metadata.Homepage }}").unwrap();
        assert_eq!(home, "https://example.com");

        let repo = ctx.render_template("{{ Metadata.Repository }}").unwrap();
        assert_eq!(repo, "https://github.com/example/test");

        let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
        assert_eq!(docs, "https://docs.example.com");

        let lic = ctx.render_template("{{ Metadata.License }}").unwrap();
        assert_eq!(lic, "MIT");

        let ts = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
        assert_eq!(ts, "1234567890");
    }

    #[test]
    fn test_populate_metadata_var_license_falls_back_to_derived() {
        // No top-level `metadata.license`: the var must derive from the
        // primary crate's Cargo.toml-derived license (here, a dual SPDX
        // expression), not render empty.
        let mut config = Config::default();
        config.crates = vec![crate::config::CrateConfig {
            name: "anodizer".to_string(),
            ..Default::default()
        }];
        config.derived_metadata.insert(
            "anodizer".to_string(),
            crate::config::MetadataConfig {
                description: Some("Derived desc".to_string()),
                homepage: Some("https://derived.example".to_string()),
                documentation: Some("https://derived.docs".to_string()),
                license: Some("MIT OR Apache-2.0".to_string()),
                ..Default::default()
            },
        );
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        assert_eq!(
            ctx.render_template("{{ Metadata.License }}").unwrap(),
            "MIT OR Apache-2.0"
        );
        assert_eq!(
            ctx.render_template("{{ Metadata.Description }}").unwrap(),
            "Derived desc"
        );
        assert_eq!(
            ctx.render_template("{{ Metadata.Homepage }}").unwrap(),
            "https://derived.example"
        );
        assert_eq!(
            ctx.render_template("{{ Metadata.Documentation }}").unwrap(),
            "https://derived.docs"
        );
    }

    #[test]
    fn test_populate_metadata_var_top_level_license_wins_over_derived() {
        // Explicit top-level `metadata.license` still wins over the derived
        // Cargo.toml value.
        let mut config = Config::default();
        config.crates = vec![crate::config::CrateConfig {
            name: "anodizer".to_string(),
            ..Default::default()
        }];
        config.derived_metadata.insert(
            "anodizer".to_string(),
            crate::config::MetadataConfig {
                license: Some("MIT OR Apache-2.0".to_string()),
                ..Default::default()
            },
        );
        config.metadata = Some(crate::config::MetadataConfig {
            license: Some("GPL-3.0".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        assert_eq!(
            ctx.render_template("{{ Metadata.License }}").unwrap(),
            "GPL-3.0"
        );
    }

    #[test]
    fn test_populate_metadata_var_documentation_renders() {
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            documentation: Some("https://docs.rs/anodizer".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
        assert_eq!(docs, "https://docs.rs/anodizer");
    }

    #[test]
    fn test_populate_metadata_var_documentation_empty_when_unset() {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.populate_metadata_var().unwrap();

        let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
        assert_eq!(docs, "");
    }

    #[test]
    fn test_populate_metadata_var_full_description_inline() {
        use crate::config::ContentSource;
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            full_description: Some(ContentSource::Inline(
                "A long-form description of the project.".to_string(),
            )),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();
        let rendered = ctx
            .render_template("{{ Metadata.FullDescription }}")
            .unwrap();
        assert_eq!(rendered, "A long-form description of the project.");
    }

    #[test]
    fn test_populate_metadata_var_full_description_from_file() {
        use crate::config::ContentSource;
        let tmp = tempfile::tempdir().unwrap();
        let desc_path = tmp.path().join("DESCRIPTION.md");
        std::fs::write(&desc_path, "read from disk").unwrap();
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            full_description: Some(ContentSource::FromFile {
                from_file: desc_path.to_string_lossy().into_owned(),
            }),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();
        let rendered = ctx
            .render_template("{{ Metadata.FullDescription }}")
            .unwrap();
        assert_eq!(rendered, "read from disk");
    }

    #[test]
    fn test_populate_metadata_var_full_description_from_url_resolves() {
        // `from_url` routes through the shared `content_source::resolve`
        // helper. We stand up a oneshot HTTP responder so the test is
        // hermetic (no real network) and verify the body lands in the
        // rendered Metadata.FullDescription variable.
        use crate::config::ContentSource;
        use crate::test_helpers::responder::spawn_oneshot_http_responder;

        let body = "long form description body";
        let body_len = body.len();
        let response: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![response]);

        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            full_description: Some(ContentSource::FromUrl {
                from_url: format!("http://{addr}/description.md"),
                headers: None,
            }),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var()
            .expect("from_url should resolve through content_source");
        let rendered = ctx
            .render_template("{{ Metadata.FullDescription }}")
            .unwrap();
        assert_eq!(rendered, body);
    }

    #[test]
    fn test_populate_metadata_var_commit_author() {
        use crate::config::CommitAuthorConfig;
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            commit_author: Some(CommitAuthorConfig {
                name: Some("Alice Developer".to_string()),
                email: Some("alice@example.com".to_string()),
                signing: None,
                use_github_app_token: false,
            }),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();
        let name = ctx
            .render_template("{{ Metadata.CommitAuthor.Name }}")
            .unwrap();
        assert_eq!(name, "Alice Developer");
        let email = ctx
            .render_template("{{ Metadata.CommitAuthor.Email }}")
            .unwrap();
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn test_artifact_id_template_var() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactID", "default");

        let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
        assert_eq!(result, "default");
    }

    #[test]
    fn test_artifact_id_empty_when_not_set() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactID", "");

        let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_pro_vars_rendered_in_templates() {
        // Test that all Pro vars can be used in templates together
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let result = ctx
            .render_template(
                "{% if IsRelease %}release{% endif %}-{% if IsMerging %}merge{% endif %}-{{ .PrefixedTag }}",
            )
            .unwrap();
        assert_eq!(result, "release-merge-api/v1.2.3");
    }

    #[test]
    fn test_is_release_without_git_info() {
        // IsRelease should still be set even without git info
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsRelease"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_is_merging_without_git_info() {
        // IsMerging should still be set even without git info
        let config = Config::default();
        let opts = ContextOptions {
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get_structured("IsMerging"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    // -----------------------------------------------------------------------
    // Monorepo template variable tests
    // -----------------------------------------------------------------------

    /// Parity proof: in monorepo mode `populate_git_vars` derives `Version`
    /// from the shared `SemVer::version_string()` helper — the SAME source the
    /// build stage's per-crate `crate_template_overrides` uses — so the two
    /// can't drift. Exercised with a prerelease + build-metadata tag, the case
    /// where the old raw string-strip and the struct derivation could diverge.
    #[test]
    fn test_monorepo_version_matches_shared_semver_helper() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("core/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let semver = SemVer {
            major: 2,
            minor: 1,
            patch: 0,
            prerelease: Some("rc.1".to_string()),
            build_metadata: Some("build.7".to_string()),
        };
        let mut info = make_git_info(false, None);
        info.tag = "core/v2.1.0-rc.1+build.7".to_string();
        info.semver = semver.clone();
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // populate_git_vars (monorepo path) and the build stage's per-crate
        // derivation both route through SemVer::version_string().
        assert_eq!(v.get("Version"), Some(&semver.version_string()));
        assert_eq!(v.get("Version"), Some(&"2.1.0-rc.1+build.7".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&semver.raw_version_string()));
        assert_eq!(v.get("RawVersion"), Some(&"2.1.0".to_string()));
        // Tag is still the monorepo-stripped value.
        assert_eq!(v.get("Tag"), Some(&"v2.1.0-rc.1+build.7".to_string()));
    }

    #[test]
    fn test_monorepo_tag_prefix_strips_tag_for_template_var() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("subproject1/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        // Simulate a monorepo tag: the full prefixed tag is stored in git_info.
        let mut info = make_git_info(false, None);
        info.tag = "subproject1/v1.2.3".to_string();
        info.previous_tag = Some("subproject1/v1.2.2".to_string());
        info.summary = "subproject1/v1.2.3-0-gabc123d".to_string();
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Tag should have the prefix stripped.
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // Version should derive from stripped tag.
        assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
        // PrefixedTag should retain the full tag.
        assert_eq!(
            v.get("PrefixedTag"),
            Some(&"subproject1/v1.2.3".to_string())
        );
        // PreviousTag should be stripped (consistent with Tag).
        assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
        // PrefixedPreviousTag should retain the full tag.
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"subproject1/v1.2.2".to_string())
        );
        // Summary should be stripped.
        assert_eq!(v.get("Summary"), Some(&"v1.2.3-0-gabc123d".to_string()));
        // PrefixedSummary should retain the full summary.
        assert_eq!(
            v.get("PrefixedSummary"),
            Some(&"subproject1/v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_monorepo_prefixed_previous_tag() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v2.0.0".to_string();
        info.previous_tag = Some("svc/v1.9.0".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // PrefixedPreviousTag should be the full previous tag.
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"svc/v1.9.0".to_string())
        );
        // PreviousTag should be stripped (prefix removed), consistent with Tag.
        assert_eq!(v.get("PreviousTag"), Some(&"v1.9.0".to_string()));
    }

    #[test]
    fn test_no_monorepo_falls_back_to_tag_prefix() {
        // When monorepo is not set, PrefixedTag should use tag.tag_prefix.
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("release/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Tag is plain "v1.2.3" (not stripped because no monorepo).
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // PrefixedTag should prepend tag_prefix.
        assert_eq!(v.get("PrefixedTag"), Some(&"release/v1.2.3".to_string()));
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"release/v1.2.2".to_string())
        );
    }

    #[test]
    fn test_monorepo_overrides_tag_prefix_for_prefixed_vars() {
        // When both monorepo.tag_prefix and tag.tag_prefix are set,
        // monorepo should take precedence for PrefixedTag.
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("release/".to_string()),
            ..Default::default()
        });
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v1.2.3".to_string();
        info.previous_tag = Some("svc/v1.2.2".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Monorepo takes precedence: Tag is stripped.
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // PrefixedTag is the full monorepo tag, NOT tag_prefix-prepended.
        assert_eq!(v.get("PrefixedTag"), Some(&"svc/v1.2.3".to_string()));
    }

    #[test]
    fn test_monorepo_prefixed_summary() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("pkg/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "pkg/v1.2.3".to_string();
        // In a real monorepo, `git describe` already includes the prefix in the summary.
        info.summary = "pkg/v1.2.3-0-gabc123d".to_string();
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        // PrefixedSummary is info.summary as-is (already contains prefix).
        assert_eq!(
            ctx.template_vars().get("PrefixedSummary"),
            Some(&"pkg/v1.2.3-0-gabc123d".to_string())
        );
        // Summary should have the prefix stripped.
        assert_eq!(
            ctx.template_vars().get("Summary"),
            Some(&"v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_monorepo_no_previous_tag() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v1.0.0".to_string();
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("PrefixedPreviousTag"), Some(&"".to_string()));
        // PreviousTag should also be empty when no previous tag exists.
        assert_eq!(v.get("PreviousTag"), Some(&"".to_string()));
    }

    // -----------------------------------------------------------------------
    // Integration test: full monorepo flow
    // -----------------------------------------------------------------------

    #[test]
    fn test_monorepo_full_flow_all_vars() {
        // End-to-end test: config with monorepo.tag_prefix + dir
        // → context creation → populate_git_vars → verify ALL template vars.
        let mut config = Config::default();
        config.project_name = "mymonorepo".to_string();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("services/api/".to_string()),
            dir: Some("services/api".to_string()),
        });

        // Verify Config helper methods work
        assert_eq!(config.monorepo_tag_prefix(), Some("services/api/"));
        assert_eq!(config.monorepo_dir(), Some("services/api"));

        let mut ctx = Context::new(config, ContextOptions::default());

        // Simulate git info as it would appear in a monorepo:
        // tag and summary already contain the prefix from git.
        let mut info = make_git_info(false, None);
        info.tag = "services/api/v2.1.0".to_string();
        info.previous_tag = Some("services/api/v2.0.5".to_string());
        info.summary = "services/api/v2.1.0-0-gabc123d".to_string();
        info.semver = crate::git::SemVer {
            major: 2,
            minor: 1,
            patch: 0,
            prerelease: None,
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();

        // Base vars should have the prefix STRIPPED.
        assert_eq!(v.get("Tag"), Some(&"v2.1.0".to_string()));
        assert_eq!(v.get("Version"), Some(&"2.1.0".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"2.1.0".to_string()));
        assert_eq!(v.get("Major"), Some(&"2".to_string()));
        assert_eq!(v.get("Minor"), Some(&"1".to_string()));
        assert_eq!(v.get("Patch"), Some(&"0".to_string()));
        assert_eq!(v.get("PreviousTag"), Some(&"v2.0.5".to_string()));
        assert_eq!(v.get("Summary"), Some(&"v2.1.0-0-gabc123d".to_string()));

        // Prefixed vars should retain the FULL prefix.
        assert_eq!(
            v.get("PrefixedTag"),
            Some(&"services/api/v2.1.0".to_string())
        );
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"services/api/v2.0.5".to_string())
        );
        assert_eq!(
            v.get("PrefixedSummary"),
            Some(&"services/api/v2.1.0-0-gabc123d".to_string())
        );

        // Project name should be available.
        assert_eq!(v.get("ProjectName"), Some(&"mymonorepo".to_string()));
    }

    #[test]
    fn render_template_for_version_blanks_semver_parts_on_non_semver() {
        // Context version is 2.0.0 — its Major/Minor/Patch must NOT leak into a
        // non-semver `--version` render.
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let vars = ctx.template_vars_mut();
        vars.set("Version", "2.0.0");
        vars.set("Major", "2");
        vars.set("Minor", "0");
        vars.set("Patch", "0");

        let rendered = ctx
            .render_template_for_version(
                "{{ .Major }}.{{ .Minor }}.{{ .Patch }}",
                "not-a-semver",
                "not-a-semver",
            )
            .expect("render");
        // The context version (parts) must not leak — semver-part vars are blanked.
        assert!(
            !rendered.contains('2') && !rendered.contains("2.0.0"),
            "context version leaked into non-semver render: {rendered:?}"
        );

        // The raw `Version` var still resolves to the supplied non-semver string.
        let version_only = ctx
            .render_template_for_version("{{ .Version }}", "not-a-semver", "vX")
            .expect("render");
        assert_eq!(version_only, "not-a-semver");
    }

    #[test]
    fn context_env_var_defaults_to_process_env_source() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        // A deliberately weird name no real shell will ever export.
        assert_eq!(ctx.env_var("ANODIZER_T3_UNSET_VAR"), None);
    }

    #[test]
    fn context_env_var_routes_to_injected_source() {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.set_env_source(crate::MapEnvSource::new().with("INJECTED", "yes"));
        assert_eq!(ctx.env_var("INJECTED"), Some("yes".to_string()));
        // The injected source REPLACES the process source — `PATH` is set
        // in every realistic execution environment, but the map does not
        // know about it, so the read must return `None`.
        assert_eq!(ctx.env_var("PATH"), None);
    }

    #[test]
    fn retry_deadline_is_some_when_config_sets_max_elapsed() {
        let mut config = Config::default();
        config.retry = Some(crate::config::RetryConfig {
            max_elapsed: Some(crate::config::HumanDuration(
                std::time::Duration::from_secs(15 * 60),
            )),
            ..Default::default()
        });
        let ctx = Context::new(config, ContextOptions::default());
        assert!(
            ctx.retry_deadline().is_some(),
            "retry.max_elapsed: 15m must resolve to a wall-clock deadline"
        );
    }

    #[test]
    fn retry_deadline_defaults_to_the_built_in_budget_when_config_omits_retry() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let before = std::time::Instant::now() + crate::retry::DEFAULT_MAX_ELAPSED;
        let deadline = ctx
            .retry_deadline()
            .expect("an omitted retry config must still yield the default budget");
        let after = std::time::Instant::now() + crate::retry::DEFAULT_MAX_ELAPSED;
        // The deadline anchors at call time + the 15m default, so it lands within
        // the [before, after] window bracketing this call.
        assert!(deadline >= before && deadline <= after);
    }

    #[test]
    #[serial_test::serial]
    fn populate_runtime_vars_sets_rustc_version() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        // RustcVersion is folded into populate_runtime_vars — exercising the
        // public entry point proves the delegation wires the var through.
        ctx.populate_runtime_vars();

        let ver = ctx
            .template_vars()
            .get("RustcVersion")
            .expect("RustcVersion should be set after populate_runtime_vars");
        // On a host with rustc on PATH the var must be non-empty and start
        // with a digit (e.g. "1.96.0").  On a host without rustc the var is
        // empty but must still be present (no missing-key footgun).
        if !ver.is_empty() {
            assert!(
                ver.chars().next().is_some_and(|c| c.is_ascii_digit()),
                "RustcVersion should start with a digit: {ver}"
            );
        }
    }
}
