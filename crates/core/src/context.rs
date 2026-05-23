use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::git::GitInfo;
use crate::log::{StageLogger, Verbosity};
use crate::partial::PartialTarget;
use crate::publish_report::PublishReport;
use crate::scm::ScmTokenType;
use crate::template::TemplateVars;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
    /// evidence is present in the report. Irreversible publishers
    /// (chocolatey moderation, winget PRs, AUR) are never rolled back —
    /// the Submitter gate is the only protection.
    #[default]
    BestEffort,
}

/// Valid --skip values for the `release` command (matches GoReleaser).
///
/// Publisher skip names use the short canonical form (matching the CLI binary
/// name and GoReleaser convention): `brew`, `choco`, `krew`, `cargo`, etc.
/// Long aliases (e.g. `homebrew`, `chocolatey`) are NOT accepted forbids
/// aliases; use the short name everywhere.
pub const VALID_RELEASE_SKIPS: &[&str] = &[
    "publish",
    "announce",
    "sign",
    "validate",
    "sbom",
    "docker",
    "docker-sign",
    "winget",
    "choco",
    "snapcraft",
    "snapcraft-publish",
    "scoop",
    "brew",
    "nix",
    "aur",
    "cargo",
    "krew",
    "nfpm",
    "makeself",
    "flatpak",
    "srpm",
    "before",
    "notarize",
    "archive",
    "source",
    "build",
    "changelog",
    "release",
    "checksum",
    "upx",
    "blob",
    "templatefiles",
    "dmg",
    "msi",
    "nsis",
    "pkg",
    "appbundle",
];

/// Valid --skip values for the `build` command.
pub const VALID_BUILD_SKIPS: &[&str] = &["pre-hooks", "post-hooks", "validate", "before"];

/// Validate that all skip values are in the allowed set.
///
/// Returns `Ok(())` if all values are valid, or `Err` with a descriptive
/// message listing the invalid value(s) and the full set of valid options.
pub fn validate_skip_values(skip: &[String], valid: &[&str]) -> Result<(), String> {
    let invalid: Vec<&str> = skip
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !valid.contains(s))
        .collect();
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid --skip value(s): {}. Valid options: {}",
            invalid.join(", "),
            valid.join(", "),
        ))
    }
}

pub struct ContextOptions {
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
    pub skip_stages: Vec<String>,
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
    /// flag in `setup_env` to defer the GitHub-token check to
    /// `publish_only::preflight_credentials`, which owns the
    /// combined token + sign-key check and bails fail-closed on
    /// missing values. Without this gate, `setup_env`'s token check
    /// would fire FIRST and pre-empt publish-only's own preflight
    /// (which validates BOTH token AND sign key in one shot).
    pub publish_only: bool,
    /// Explicit project root directory. When set, stages use this instead of
    /// discovering the repo root via `git rev-parse --show-toplevel`.
    pub project_root: Option<PathBuf>,
    /// Strict mode: configured features that would silently skip become errors.
    pub strict: bool,
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
            selected_crates: Vec::new(),
            token: None,
            parallelism: 4,
            single_target: None,
            release_notes_path: None,
            fail_fast: false,
            partial_target: None,
            merge: false,
            publish_only: false,
            project_root: None,
            strict: false,
            resume_release: false,
            replace_existing_artifacts: false,
            skip_post_publish_poll: false,
            gate_submitter: None,
            rollback_mode: None,
            simulate_failure_publishers: Vec::new(),
            rollback_only: false,
            allow_rerun: false,
            from_run: None,
            runtime_nondeterministic_allowlist: Vec::new(),
            summary_json_path: None,
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
    /// body (matching GoReleaser's `loadContent(ReleaseHeader…)` behaviour).
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
    /// GoReleaser's `pipe.SkipMemento` pattern. The inner `Arc<Mutex<…>>`
    /// lets parallel stage workers contribute without extra plumbing.
    pub skip_memento: crate::pipe_skip::SkipMemento,
    /// Trait-based publisher dispatch report, set by `PublishStage::run`
    /// when the per-publisher dispatcher finishes. `None` until the
    /// publish stage executes (or when publishing is skipped entirely
    /// via snapshot mode / `--skip=publish`). Downstream stages
    /// (SnapcraftPublishStage, AnnounceStage, future Submitter-group
    /// stages) consult this to apply the submitter-gate / announce-gate
    /// rules — see `PublishReport::any_failed`.
    pub publish_report: Option<PublishReport>,
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
    /// Optional in-memory log-capture handle. When `Some`, every logger
    /// produced by [`Context::logger`] attaches it so the test can read
    /// back aggregated counts of `status` / `warn` / etc. calls without
    /// having to intercept stderr. `None` in production (no overhead).
    pub log_capture: Option<crate::log::LogCapture>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            stage_outputs: StageOutputs::default(),
            template_vars: vars,
            git_info: None,
            token_type: ScmTokenType::GitHub,
            skip_memento: crate::pipe_skip::SkipMemento::new(),
            publish_report: None,
            determinism: None,
            pending_outcome: None,
            log_capture: None,
        }
    }

    /// Attach an in-memory log-capture sink so every logger derived from
    /// this context via [`Context::logger`] records to it. Intended for
    /// tests; production callers leave this `None`.
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

    /// Borrow the publisher dispatch report set by `PublishStage::run`,
    /// or `None` if the publish stage hasn't run yet (or was skipped).
    pub fn publish_report(&self) -> Option<&PublishReport> {
        self.publish_report.as_ref()
    }

    /// Store the publisher dispatch report. Overwrites any prior report;
    /// the publish stage is the single writer.
    pub fn set_publish_report(&mut self, r: PublishReport) {
        self.publish_report = Some(r);
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

    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
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

    pub fn is_strict(&self) -> bool {
        self.options.strict
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
            log.status(&format!("{}: skipped (snapshot mode)", stage));
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
                log.warn(&format!("{}: failed to render template: {}", label, e));
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

    /// Create a [`StageLogger`] for the given stage name, pre-attached to
    /// the context's env-pairs list so that subprocess stderr / stdout
    /// flowing through [`StageLogger::check_output`] is automatically
    /// redacted. The env list combines the template-engine env
    /// (process + config + `.env` files) and the current `std::env::vars`
    /// snapshot, so any secret value reachable to a hook or subprocess is
    /// available for scrubbing.
    pub fn logger(&self, stage: &'static str) -> StageLogger {
        let mut log = StageLogger::new(stage, self.verbosity()).with_env(self.env_for_redact());
        if let Some(cap) = &self.log_capture {
            log = log.with_capture_handle(cap.clone());
        }
        log
    }

    /// Build the env-pairs list used to seed every [`StageLogger`] created
    /// via [`Context::logger`]. Combines the template-engine env map
    /// (process env + config env + `.env` file values) with the current
    /// `std::env::vars` snapshot, deduplicating by key (template-engine
    /// values win because they reflect any user overrides).
    fn env_for_redact(&self) -> Vec<(String, String)> {
        use std::collections::HashMap;
        let mut map: HashMap<String, String> = std::env::vars().collect();
        for (k, v) in self.template_vars.all_env() {
            map.insert(k.clone(), v.clone());
        }
        map.into_iter().collect()
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
            // RawVersion: just major.minor.patch, no prerelease or build metadata.
            let raw_version = format!(
                "{}.{}.{}",
                info.semver.major, info.semver.minor, info.semver.patch
            );

            // Version: clean semver derived from the parsed SemVer struct, not
            // from the tag string.  The old `tag.strip_prefix('v')` approach
            // broke for monorepo workspace tags like `core-v0.3.2` because it
            // only stripped a leading 'v', leaving `core-v0.3.2` intact.
            // Deriving from the struct handles all tag_template prefixes.
            let mut version = raw_version.clone();
            if let Some(ref pre) = info.semver.prerelease {
                version.push('-');
                version.push_str(pre);
            }
            if let Some(ref meta) = info.semver.build_metadata {
                version.push('+');
                version.push_str(meta);
            }

            self.template_vars.set("Tag", &info.tag);
            self.template_vars.set("Version", &version);
            self.template_vars.set("RawVersion", &raw_version);
            self.template_vars
                .set("Major", &info.semver.major.to_string());
            self.template_vars
                .set("Minor", &info.semver.minor.to_string());
            self.template_vars
                .set("Patch", &info.semver.patch.to_string());
            self.template_vars.set(
                "Prerelease",
                info.semver.prerelease.as_deref().unwrap_or(""),
            );
            self.template_vars.set(
                "BuildMetadata",
                info.semver.build_metadata.as_deref().unwrap_or(""),
            );
            self.template_vars.set("FullCommit", &info.commit);
            self.template_vars.set("Commit", &info.commit);
            self.template_vars.set("ShortCommit", &info.short_commit);
            self.template_vars.set("Branch", &info.branch);
            self.template_vars.set("CommitDate", &info.commit_date);
            self.template_vars
                .set("CommitTimestamp", &info.commit_timestamp);
            self.template_vars
                .set("IsGitDirty", if info.dirty { "true" } else { "false" });
            self.template_vars
                .set("IsGitClean", if info.dirty { "false" } else { "true" });
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

                // Version: derive from the stripped tag (overrides the initial
                // value set above from info.tag, which in monorepo mode still
                // contains the prefix).
                let version = stripped_tag
                    .strip_prefix('v')
                    .unwrap_or(stripped_tag)
                    .to_string();
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

        self.template_vars.set(
            "IsSnapshot",
            if self.options.snapshot {
                "true"
            } else {
                "false"
            },
        );
        self.template_vars.set(
            "IsNightly",
            if self.options.nightly {
                "true"
            } else {
                "false"
            },
        );
        // Surfaced to user `if_condition:` templates so stages can
        // selectively run inside the determinism harness even when
        // `IsSnapshot == "false"` would otherwise skip them.
        self.template_vars.set(
            "IsHarness",
            if std::env::var_os("ANODIZER_IN_DETERMINISM_HARNESS").is_some() {
                "true"
            } else {
                "false"
            },
        );
        // Wire IsDraft from config (GoReleaser reads ctx.Config.Release.Draft).
        let is_draft = self
            .config
            .release
            .as_ref()
            .and_then(|r| r.draft)
            .unwrap_or(false);
        self.template_vars
            .set("IsDraft", if is_draft { "true" } else { "false" });
        self.template_vars.set(
            "IsSingleTarget",
            if self.options.single_target.is_some() {
                "true"
            } else {
                "false"
            },
        );

        // Pro addition: IsRelease — true if this is a regular release (not snapshot, not nightly).
        let is_release = !self.options.snapshot && !self.options.nightly;
        self.template_vars
            .set("IsRelease", if is_release { "true" } else { "false" });

        // Pro addition: IsMerging — true if running with --merge flag.
        self.template_vars.set(
            "IsMerging",
            if self.options.merge { "true" } else { "false" },
        );
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
    /// 2. `chrono::Utc::now()` — wall-clock fallback. Matches GoReleaser's
    ///    legacy semantics for runs without SDE wired in. Note that the
    ///    GoReleaser template docs explicitly call `.Now` "not deterministic"
    ///    — under SDE-aware reproducible builds we deviate from that
    ///    behavior intentionally.
    pub fn populate_time_vars(&mut self) {
        // Resolution order (SDE first, else wall-clock) is centralized in
        // `crate::sde::resolve_now` so any caller — `populate_time_vars`,
        // Tera built-ins, stage-srpm's `%changelog` date, nightly
        // `date_str` — sees identical "now" semantics. Earlier this
        // function inlined the resolution and drifted from the helper.
        let now = crate::sde::resolve_now();
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
    /// - `Runtime_Goos` / `Runtime_Goarch` — GoReleaser-compatible nested aliases
    pub fn populate_runtime_vars(&mut self) {
        let goos = map_os_to_goos(std::env::consts::OS);
        let goarch = map_arch_to_goarch(std::env::consts::ARCH);
        self.template_vars.set("RuntimeGoos", goos);
        self.template_vars.set("RuntimeGoarch", goarch);
        // GoReleaser uses Runtime.Goos / Runtime.Goarch — after preprocessing
        // the dot becomes an underscore-separated flat key. We expose both forms.
        self.template_vars.set("Runtime_Goos", goos);
        self.template_vars.set("Runtime_Goarch", goarch);
    }

    /// Populate the `ReleaseNotes` template variable from stored changelogs.
    ///
    /// Should be called after the changelog stage has run and populated
    /// `self.stage_outputs.changelogs`. Uses the first crate (by config
    /// order) whose changelog is present, or an empty string if no
    /// changelogs exist. Config order is deterministic, unlike HashMap
    /// iteration order.
    pub fn populate_release_notes_var(&mut self) {
        // Look up changelogs in config-defined crate order for determinism.
        let notes = self
            .config
            .crates
            .iter()
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
        // template-exposed view is expanded. Matches GoReleaser's
        // ExtraBinaries / ExtraFiles list semantics.
        const CSV_LIST_KEYS: &[&str] = &["extra_binaries", "extra_files"];

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
        // serde_json::Value and tera::Value are the same type under the hood,
        // so no conversion is needed — pass values directly.
        let tera_value = tera::Value::Array(artifacts_value);
        self.template_vars.set_structured("Artifacts", tera_value);
    }

    /// Populate the `Metadata` structured template variable from config.metadata.
    ///
    /// Exposes the project metadata block as a nested map with PascalCase keys
    /// matching GoReleaser's `.Metadata.*` namespace:
    /// `Description`, `Homepage`, `License`, `Maintainers`, `ModTimestamp`,
    /// `FullDescription` (resolved), `CommitAuthor.{Name,Email}`.
    /// Missing fields default to empty strings / empty arrays.
    ///
    /// `full_description` with `from_url` is NOT resolved here (avoids a
    /// reqwest dep in core); the FromUrl case returns an error and the caller
    /// should surface it. Inline and FromFile are resolved synchronously.
    pub fn populate_metadata_var(&mut self) -> anyhow::Result<()> {
        use crate::config::ContentSource;

        // Clone the small scalar fields so we don't hold a borrow on self.config
        // across the render_template calls below.
        let (
            description,
            homepage,
            license,
            maintainers,
            mod_timestamp,
            full_desc_src,
            commit_author,
        ) = {
            let meta = self.config.metadata.as_ref();
            let description = meta
                .and_then(|m| m.description.as_deref())
                .unwrap_or("")
                .to_string();
            let homepage = meta
                .and_then(|m| m.homepage.as_deref())
                .unwrap_or("")
                .to_string();
            let license = meta
                .and_then(|m| m.license.as_deref())
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
                license,
                maintainers,
                mod_timestamp,
                full_desc_src,
                commit_author,
            )
        };

        // Resolve full_description (Inline + FromFile in-core; FromUrl errors here).
        let full_description = match full_desc_src {
            None => String::new(),
            Some(ContentSource::Inline(s)) => s,
            Some(ContentSource::FromFile { from_file }) => {
                let rendered_path = self.render_template(&from_file).with_context(|| {
                    format!("metadata.full_description: render path '{}'", from_file)
                })?;
                std::fs::read_to_string(&rendered_path).with_context(|| {
                    format!(
                        "metadata.full_description: read from_file '{}'",
                        rendered_path
                    )
                })?
            }
            Some(ContentSource::FromUrl { .. }) => {
                anyhow::bail!(
                    "metadata.full_description: `from_url` is not yet supported at metadata \
                     population time (core has no HTTP client). Use `from_file` with a \
                     pre-fetched file, or inline the content. Tracked for future: move \
                     URL resolution into a late-pipeline stage or add reqwest to core."
                );
            }
        };

        let commit_author_map = serde_json::json!({
            "Name": commit_author.as_ref().and_then(|c| c.name.clone()).unwrap_or_default(),
            "Email": commit_author.as_ref().and_then(|c| c.email.clone()).unwrap_or_default(),
        });

        let meta_map = serde_json::json!({
            "Description": description,
            "Homepage": homepage,
            "License": license,
            "Maintainers": maintainers,
            "ModTimestamp": mod_timestamp,
            "FullDescription": full_description,
            "CommitAuthor": commit_author_map,
        });
        // serde_json::Value and tera::Value are the same type, so pass directly.
        self.template_vars.set_structured("Metadata", meta_map);
        Ok(())
    }
}

/// Map Rust's `std::env::consts::OS` to Go-compatible GOOS naming.
/// GoReleaser templates expect Go runtime names (e.g. "darwin" not "macos").
pub fn map_os_to_goos(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other, // linux, windows, freebsd, etc. already match
    }
}

/// Map Rust's `std::env::consts::ARCH` to Go-compatible GOARCH naming.
/// GoReleaser templates expect Go runtime names (e.g. "amd64" not "x86_64").
pub fn map_arch_to_goarch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        "mips" => "mips",
        "mips64" => "mips64",
        "riscv64" => "riscv64",
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::{GitInfo, SemVer};

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
        assert_eq!(v.get("IsGitDirty"), Some(&"false".to_string()));
        assert_eq!(v.get("GitTreeState"), Some(&"clean".to_string()));
    }

    #[test]
    fn test_git_tree_state_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("IsGitDirty"), Some(&"true".to_string()));
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
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
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
            ctx2.template_vars().get("IsSnapshot"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_draft_defaults_to_false() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
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
    #[serial_test::serial(env)]
    fn populate_time_vars_uses_source_date_epoch_when_set() {
        let key = "SOURCE_DATE_EPOCH";
        let prev = std::env::var(key).ok();
        // 1_715_000_000 = 2024-05-06T12:53:20+00:00 — picked to be safely
        // earlier than wall-clock so a wall-clock-derived assertion would
        // visibly fail.
        // SAFETY: serialized on the `env` lock group.
        unsafe { std::env::set_var(key, "1715000000") };

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
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

        // SAFETY: serialized.
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn test_populate_time_vars() {
        // Wall-clock fallback path: clear SDE so we exercise the
        // chrono::Utc::now() branch.
        let key = "SOURCE_DATE_EPOCH";
        let prev = std::env::var(key).ok();
        // SAFETY: serialized.
        unsafe { std::env::remove_var(key) };

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
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

        // SAFETY: serialized.
        unsafe {
            if let Some(v) = prev {
                std::env::set_var(key, v);
            }
        }
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
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
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
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
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
            ctx.template_vars().get("IsNightly"),
            Some(&"false".to_string()),
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
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
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
            ctx.template_vars().get("IsGitClean"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_git_clean_when_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsGitClean"),
            Some(&"false".to_string())
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
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"false".to_string())
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
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"true".to_string())
        );
    }

    #[test]
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
        // (not just the prefix), matching GoReleaser behavior.
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
            ctx.template_vars().get("IsRelease"),
            Some(&"true".to_string())
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
            ctx.template_vars().get("IsRelease"),
            Some(&"false".to_string())
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
            ctx.template_vars().get("IsRelease"),
            Some(&"false".to_string())
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
            ctx.template_vars().get("IsMerging"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_merging_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsMerging"),
            Some(&"false".to_string())
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
            license: Some("MIT".to_string()),
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

        let lic = ctx.render_template("{{ Metadata.License }}").unwrap();
        assert_eq!(lic, "MIT");

        let ts = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
        assert_eq!(ts, "1234567890");
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
    fn test_populate_metadata_var_full_description_from_url_errors() {
        // Avoids silent-skip footgun (see W1 in pro-features-audit.md). If the user
        // configures from_url for metadata.full_description, emit a clear, actionable
        // error at context-populate time rather than quietly shipping an empty string.
        use crate::config::ContentSource;
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            full_description: Some(ContentSource::FromUrl {
                from_url: "https://example.com/description.md".to_string(),
                headers: None,
            }),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        let err = ctx
            .populate_metadata_var()
            .expect_err("from_url must error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("metadata.full_description") && msg.contains("from_url"),
            "error should mention the feature + limitation, got: {msg}"
        );
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
            ctx.template_vars().get("IsRelease"),
            Some(&"true".to_string())
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
            ctx.template_vars().get("IsMerging"),
            Some(&"true".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Monorepo template variable tests
    // -----------------------------------------------------------------------

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
}
