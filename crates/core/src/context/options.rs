use super::*;

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
