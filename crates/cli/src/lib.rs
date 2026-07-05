use clap::{Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

/// Shared `--publishers` help stem used across `release`, `publish`, and
/// `check config` so the flag presents one mental model on every command.
/// `check config` appends its validate-only clause (see its `#[arg]`).
const PUBLISHERS_HELP_STEM: &str = "Comma-separated publishers to run (default: all configured). \
     --skip always wins over --publishers.";

/// Shared `--token` help used by every token-taking subcommand, rendered
/// from the canonical env ladder so the documented override order can never
/// drift from the order the resolver actually applies.
static TOKEN_HELP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "GitHub token (overrides {} env vars)",
        anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / ")
    )
});

/// `--prepare` help, rendered from `UPSTREAM_STAGES` so the documented skip
/// set can never drift from the set the flag actually skips.
static PREPARE_HELP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "Run local build + archive + sign + checksum + sbom stages but skip every \
         upstream-reaching stage ({}) — GoReleaser Pro parity. Artifacts stay in dist/ \
         for inspection. `--prepare-only` is accepted as an alias for GR-imported scripts.",
        anodizer_core::stages::UPSTREAM_STAGES.join(", ")
    )
});

#[derive(Parser)]
#[command(name = "anodizer", version, about = "Release Rust projects with ease")]
pub struct Cli {
    #[arg(
        long,
        short = 'f',
        global = true,
        help = "Path to config file (overrides auto-detection)"
    )]
    pub config: Option<PathBuf>,
    #[arg(long, global = true, help = "Enable verbose output")]
    pub verbose: bool,
    #[arg(long, global = true, help = "Enable debug output")]
    pub debug: bool,
    #[arg(long, short = 'q', global = true, help = "Suppress non-error output")]
    pub quiet: bool,
    #[arg(
        long,
        global = true,
        help = "Strict mode: configured features that silently skip become hard errors"
    )]
    pub strict: bool,
    // Optional so `anodizer` with no args prints help and exits 0. A required
    // subcommand (non-Option) makes clap emit a "usage" error and exit with
    // code 2, which package-manager validators (winget's, chocolatey's) treat
    // as install failure since they smoke-test the installed binary with no
    // args.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
// The `Release` variant carries one field per CLI flag (~40 fields) so its
// size dwarfs the other subcommands. Boxing every flag bag would just hide
// the same fields behind an extra allocation per parse with no callsite
// win; the enum is allocated once per invocation. Local allow only.
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Run the full release pipeline
    Release {
        #[arg(long = "crate", visible_alias = "id", action = clap::ArgAction::Append, help = "Release a specific crate (repeatable; --id is accepted as a GoReleaser-compat alias)")]
        crate_names: Vec<String>,
        #[arg(long, help = "Release all crates with unreleased changes")]
        all: bool,
        #[arg(long, help = "Force release even without unreleased changes")]
        force: bool,
        #[arg(long, help = "Build without publishing (snapshot mode)")]
        snapshot: bool,
        #[arg(long, help = "Create a nightly release with date-based version")]
        nightly: bool,
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(long, help = "Remove dist directory before starting")]
        clean: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Skip stages or publishers (comma-separated, e.g. docker,announce,npm). \
                    Unified denylist: a stage name skips the stage, a publisher name \
                    (npm, homebrew, chocolatey, …) skips that publisher."
        )]
        skip: Vec<String>,
        #[arg(long = "publishers", value_delimiter = ',', help = PUBLISHERS_HELP_STEM)]
        publishers: Vec<String>,
        #[arg(
            long,
            help = TOKEN_HELP.as_str()
        )]
        token: Option<String>,
        #[arg(
            long,
            default_value = "3h",
            help = "Pipeline timeout duration (e.g., 90m, 3h, 5s) — a generous \
                    safety backstop, not the primary bound; per-stage bounds \
                    (e.g. announce.deadline) catch a hung stage in seconds"
        )]
        timeout: String,
        #[arg(
            long,
            short = 'p',
            help = "Maximum number of parallel build jobs (default: number of CPUs)"
        )]
        parallelism: Option<usize>,
        #[arg(long, help = "Automatically set --snapshot if the git repo is dirty")]
        auto_snapshot: bool,
        #[arg(long, help = "Build only for the host target triple")]
        single_target: bool,
        #[arg(
            long,
            value_name = "csv",
            conflicts_with = "single_target",
            help = "Restrict the build to a comma-separated subset of configured target triples (e.g. x86_64-apple-darwin,aarch64-apple-darwin). Used by the Determinism Harness's sharded job matrix; conflicts with --single-target."
        )]
        targets: Option<String>,
        #[arg(
            long = "host-targets",
            conflicts_with = "single_target",
            conflicts_with = "targets",
            help = "Build every configured target this host can build, skipping cross-compile-only targets (apple targets on a non-macOS host). Only valid with --snapshot or --dry-run. Used by `task prepush` to do a real host-scoped build without aborting on un-buildable targets."
        )]
        host_targets: bool,
        #[arg(
            long,
            help = "Path to a custom release notes file (overrides changelog)"
        )]
        release_notes: Option<PathBuf>,
        #[arg(
            long,
            conflicts_with = "crate_names",
            help = "Release a specific workspace in a monorepo config"
        )]
        workspace: Option<String>,
        #[arg(
            long,
            conflicts_with = "no_preflight",
            help = "Run pre-flight publisher-state check and exit (don't start the pipeline)"
        )]
        preflight: bool,
        #[arg(
            long,
            conflicts_with = "preflight",
            help = "Skip the automatic pre-flight publisher-state check"
        )]
        no_preflight: bool,
        #[arg(
            long = "preflight-secrets",
            conflicts_with = "no_preflight",
            help = "Validate that all required publish secrets / credentials are present (and key material is well-formed) without checking host-local tools — for a central pre-release gate across decoupled CI runners. Checks and exits; does not start the pipeline."
        )]
        preflight_secrets: bool,
        #[arg(
            long,
            help = "Alias for --strict (also treats Unknown publisher state as a blocker during pre-flight)"
        )]
        strict_preflight: bool,
        #[arg(long, help = "Set the release as a draft")]
        draft: bool,
        #[arg(long, help = "Path to a file containing custom release header text")]
        release_header: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to a template file for release header (rendered with template variables)"
        )]
        release_header_tmpl: Option<PathBuf>,
        #[arg(long, help = "Path to a file containing custom release footer text")]
        release_footer: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to a template file for release footer (rendered with template variables)"
        )]
        release_footer_tmpl: Option<PathBuf>,
        #[arg(
            long,
            help = "Path to a template file for release notes (rendered with template variables, overrides --release-notes)"
        )]
        release_notes_tmpl: Option<PathBuf>,
        #[arg(long, help = "Abort immediately on first error during publishing")]
        fail_fast: bool,
        #[arg(
            long = "no-gate-submitter",
            help = "Disable the Submitter gate: dispatch Submitter publishers even when required Assets/Manager publishers failed"
        )]
        no_gate_submitter: bool,
        #[arg(
            long = "rollback",
            value_name = "none|best-effort",
            help = "Rollback policy after publish stage. Defaults to best-effort when preflight is clean, none otherwise."
        )]
        rollback: Option<String>,
        #[arg(
            long = "simulate-failure",
            value_name = "publisher",
            action = clap::ArgAction::Append,
            hide = true,
            help = "(TEST HARNESS) Force a named publisher to fail. Gated by ANODIZE_TEST_HARNESS=1."
        )]
        simulate_failure: Vec<String>,
        #[arg(
            long = "rollback-only",
            requires = "from_run",
            conflicts_with = "clean",
            help = "Skip publish; re-attempt rollback from a prior run report. Requires --from-run=<id>."
        )]
        rollback_only: bool,
        #[arg(
            long = "from-run",
            value_name = "id",
            requires = "rollback_only",
            value_parser = parse_run_id,
            help = "Prior run id whose state to load when running --rollback-only. \
                    Loads <dist>/run-<id>/rollback.json if present (a prior replay's state), \
                    otherwise <dist>/run-<id>/report.json. Delete rollback.json to force a \
                    full re-roll. Must match the run_id format written by the release pipeline \
                    (alphanumeric, dot, dash, underscore; no path separators)."
        )]
        from_run: Option<String>,
        #[arg(
            long = "allow-rerun",
            conflicts_with = "rollback_only",
            help = "DANGEROUS: force publish to proceed even when a prior \
                    dist/run-<id>/report.json exists for this tag. PR-based publishers \
                    (homebrew, scoop, nix, krew, MCP) will open DUPLICATE pull requests. \
                    Recover from partial failures with --rollback-only --from-run=<id> first. \
                    Cannot be combined with --rollback-only (which has its own idempotency)."
        )]
        allow_rerun: bool,
        #[arg(
            long = "show-skipped",
            help = "Show per-crate 'no <publisher> config block' skip lines at default verbosity \
                    (normally only visible with --debug). Use to diagnose why a publisher didn't \
                    run for a given crate."
        )]
        show_skipped: bool,
        #[arg(
            long = "allow-nondeterministic",
            value_name = "name=reason",
            action = clap::ArgAction::Append,
            help = "Runtime non-determinism opt-out for a specific artifact (repeatable). Mutually exclusive with --strict."
        )]
        allow_nondeterministic: Vec<String>,
        #[arg(
            long = "summary-json",
            value_name = "path",
            help = "Write the per-publisher run summary JSON to this path. Without it, real (non-snapshot, non-dry-run) releases write <dist>/run-<id>/summary.json — even when a stage fails — so recovery tooling always has machine-readable publish state."
        )]
        summary_json: Option<PathBuf>,
        #[arg(
            long = "allow-ai-failure",
            help = "If `changelog.ai` is configured and the AI provider fails, log a warning and keep the pre-AI release notes instead of aborting the release."
        )]
        allow_ai_failure: bool,
        #[arg(
            long = "allow-snapshot-publish",
            help = "DANGEROUS: allow publishing a non-release version (snapshot / dirty / 0.0.0-sentinel, e.g. 0.0.0~SNAPSHOT-<sha>) to external publishers. By default the publish, blob, and announce stages refuse such versions — several indexes (crates.io, Cloudsmith, Chocolatey, winget, AUR) are one-way doors. Use ONLY for a private/test channel."
        )]
        allow_snapshot_publish: bool,
        #[arg(
            long,
            conflicts_with = "merge",
            help = "Run only the build stage for split CI fan-out (outputs artifacts JSON to dist/)"
        )]
        split: bool,
        #[arg(
            long,
            conflicts_with = "split",
            help = "Merge artifacts from split build jobs and resume the pipeline from post-build stages"
        )]
        merge: bool,
        #[arg(
            long = "publish-only",
            conflicts_with_all = ["split", "merge", "prepare", "announce_only", "snapshot", "rollback_only", "clean"],
            help = "Load artifacts from dist/ (preserved by `anodize check determinism --preserve-dist`) and run only the sign + publish pipeline. Skips build/archive/nfpm/sbom/checksum — those stages' outputs must already be present in dist/."
        )]
        publish_only: bool,
        #[arg(
            long,
            alias = "prepare-only",
            conflicts_with_all = ["publish_only", "announce_only", "rollback_only"],
            help = PREPARE_HELP.as_str()
        )]
        prepare: bool,
        #[arg(
            long = "announce-only",
            conflicts_with_all = ["prepare", "publish_only", "snapshot", "rollback_only", "split", "merge", "clean"],
            help = "Re-fire announcers only. Loads `<dist>/run-<id>/report.json` written by a prior run, skips every pipeline stage except announce (which itself short-circuits on nightly), then runs after-hooks. Use this to retry a transient announcer failure (Slack 502, Discord 5xx) without re-creating the GitHub release or re-publishing to package managers. Fails fast when no `<dist>/run-<id>/report.json` is present."
        )]
        announce_only: bool,
        #[arg(
            long,
            help = "Resume into an existing release left over from a prior failed attempt; bypasses the safety check that bails on partial assets."
        )]
        resume_release: bool,
        #[arg(
            long,
            help = "Force release.replace_existing_artifacts: true regardless of config (overwrite conflicting assets on retry)."
        )]
        replace_existing: bool,
        #[arg(
            long = "no-post-publish-poll",
            help = "Skip post-publish polling for chocolatey moderation / winget PR validation; report NotPolled for affected publishers."
        )]
        no_post_publish_poll: bool,
        #[arg(
            long = "no-failure-policy",
            hide = true,
            help = "(HARNESS) Disable the release.on_failure rollback/hold policy. Set by the determinism harness, whose hermetic replica builds nothing upstream and must surface a stage failure plainly without touching tags or the source repo."
        )]
        no_failure_policy: bool,
    },
    /// Build binaries only (always runs in snapshot mode)
    Build {
        #[arg(long = "crate", action = clap::ArgAction::Append, help = "Build a specific crate (repeatable)")]
        crate_names: Vec<String>,
        #[arg(
            long,
            default_value = "3h",
            help = "Pipeline timeout duration (e.g., 90m, 3h, 5s) — a generous \
                    safety backstop, not the primary bound; per-stage bounds \
                    (e.g. announce.deadline) catch a hung stage in seconds"
        )]
        timeout: String,
        #[arg(
            long,
            short = 'p',
            help = "Maximum number of parallel build jobs (default: number of CPUs)"
        )]
        parallelism: Option<usize>,
        #[arg(long, help = "Build only for the host target triple")]
        single_target: bool,
        #[arg(
            long,
            conflicts_with = "crate_names",
            help = "Build a specific workspace in a monorepo config"
        )]
        workspace: Option<String>,
        #[arg(
            long,
            short = 'o',
            help = "Copy the built binary to this path (requires --single-target and single crate)"
        )]
        output: Option<PathBuf>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Skip stages (comma-separated: pre-hooks, post-hooks, validate, before)"
        )]
        skip: Vec<String>,
    },
    /// Validate configuration and run determinism checks
    Check {
        #[command(subcommand)]
        cmd: CheckCmd,
    },
    /// Generate starter config, or enroll version-bearing files
    Init {
        #[arg(
            long,
            help = "Discover repo files that embed the current version and enroll the selection into version_files in .anodizer.yaml"
        )]
        version_files: bool,
        #[arg(
            long,
            value_delimiter = ',',
            requires = "version_files",
            help = "Glob(s) to drop from discovered candidates (repeatable or comma-separated); only with --version-files"
        )]
        exclude: Vec<String>,
        #[arg(
            long,
            short = 'y',
            requires = "version_files",
            help = "Non-interactive: enroll all discovered candidates without prompting"
        )]
        yes: bool,
    },
    /// Manage CHANGELOG.md: refresh the pending section, or render notes/JSON
    Changelog {
        #[arg(
            value_name = "tag|range",
            help = "Commit range to render: a single tag (predecessor-resolved against its crate), an explicit `from..to` range, or omitted to refresh each crate's pending section against its last tag"
        )]
        range: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value = "keep-a-changelog",
            help = "Output format: keep-a-changelog (refresh the [Unreleased] section), release-notes (grouped-bullet GitHub body to stdout), or json"
        )]
        format: ChangelogFormat,
        #[arg(
            long,
            help = "Apply the regenerated [Unreleased] section to the configured CHANGELOG.md file(s) in place (keep-a-changelog only)"
        )]
        write: bool,
        #[arg(long = "crate", help = "Restrict to a specific crate in a workspace")]
        crate_name: Option<String>,
        #[arg(
            long,
            help = "Preview as a snapshot release (release-notes format only)"
        )]
        snapshot: bool,
    },
    /// Generate shell completions
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    /// Check availability of required external tools
    Healthcheck,
    /// Verify the environment can run the configured release: required
    /// tools, env vars/secrets (presence only — values are never printed),
    /// endpoint reachability, docker daemon, and loadable key material,
    /// all derived from the resolved config. Every failure is reported in
    /// one pass and the exit code is non-zero when anything is missing.
    /// The same checks run automatically at the start of `anodizer release`.
    Preflight {
        #[arg(long, help = "Output the report as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Check only the publish-time surface (the stages `release --publish-only` runs), not artifact-producing stages"
        )]
        publish_only: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Skip requirement collection for these stages (comma-separated, same names as release --skip)"
        )]
        skip: Vec<String>,
        #[arg(long = "publishers", value_delimiter = ',', help = PUBLISHERS_HELP_STEM)]
        publishers: Vec<String>,
        #[arg(
            long,
            help = "GitHub token override; when set, GitHub token env-var requirements are treated as satisfied"
        )]
        token: Option<String>,
    },
    /// Generate man pages to stdout
    Man,
    /// Output JSON Schema for .anodizer.yaml
    Jsonschema,
    /// Resolve a git tag to its matching crate in the config
    ResolveTag {
        #[arg(help = "Tag to resolve (e.g. 'v1.2.3', 'core-v0.2.3')")]
        tag: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Emit the configured build targets as a GitHub Actions matrix.
    ///
    /// Derives `{os, target, artifact}` entries from `.anodizer.yaml`.
    /// Consumed by `anodizer-action`'s `split-matrix` output to feed a
    /// `strategy.matrix` dynamically (via `fromJson`).
    Targets {
        #[arg(long, help = "Output as JSON (include-form matrix)")]
        json: bool,
        #[arg(long = "crate", action = clap::ArgAction::Append, help = "Restrict to specific crate(s)")]
        crate_names: Vec<String>,
    },
    /// Emit the canonical `--skip` / `--publishers` token vocabulary.
    ///
    /// Lists every legal `--skip` / `--publishers` token, each tagged with
    /// `is_publisher` / `is_publish_stage`, derived from anodizer's publisher
    /// registry (no hand-maintained list). Consumed by `anodizer-action` so it
    /// emits only canonical tokens (e.g. `homebrew`, not `homebrew-cask`)
    /// instead of re-deriving them in shell.
    Vocabulary {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Emit the external CLI tools the resolved config's pipeline will invoke.
    ///
    /// Derives the tool set from the same per-stage / per-publisher
    /// requirements the preflight engine checks, so it tracks the config
    /// exactly. Consumed by `anodizer-action` to decide what to install on a
    /// runner instead of grepping the config in shell.
    Tools {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(
            long,
            help = "Only the tools the publish-time surface needs (the stages `release --publish-only` runs), not artifact-producing stages"
        )]
        publish_only: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Drop tools contributed by these skipped stages (comma-separated, same names as release --skip)"
        )]
        skip: Vec<String>,
        #[arg(long = "publishers", value_delimiter = ',', help = PUBLISHERS_HELP_STEM)]
        publishers: Vec<String>,
    },
    /// Auto-tag based on commit message directives
    Tag {
        #[arg(long, help = "Show what tag would be created without pushing")]
        dry_run: bool,
        #[arg(long, help = "Override bump logic with a specific tag value")]
        custom_tag: Option<String>,
        /// Tag exactly this semver version, bypassing autotag derivation and the
        /// Cargo.toml-ahead guard.
        ///
        /// Accepts `1.2.3` or `v1.2.3` (the `v`/configured prefix is normalized).
        /// The version is applied to the tag AND synced into the relevant
        /// `Cargo.toml` / `version_files` (single-crate, `--crate`, and lockstep
        /// modes). In per-crate workspace mode it is rejected unless `--crate
        /// <name>` selects a single crate — one version across independently
        /// versioned crates would corrupt their cadences. Intended for release
        /// recovery where the operator must pin a precise version.
        #[arg(long = "version", value_name = "VERSION")]
        version_override: Option<String>,
        #[arg(long, help = "Override default bump type (patch/minor/major)")]
        default_bump: Option<String>,
        #[arg(long = "crate", help = "Tag a specific crate in a workspace")]
        crate_name: Option<String>,
        #[arg(
            long,
            help = "Push the version-sync bump commit to the release branch atomically with the tag"
        )]
        push: bool,
        #[arg(
            long,
            conflicts_with = "push",
            help = "Push the tag only, leaving the version-sync bump commit local"
        )]
        no_push: bool,
        #[arg(
            long,
            value_name = "NAME",
            help = "Remote to push to (default: origin)"
        )]
        push_remote: Option<String>,
        #[arg(
            long,
            help = "Create the tag + bump commit locally but only print (not run) the git push commands --push would use; pass --dry-run to also preview tagging"
        )]
        push_dry_run: bool,
        #[arg(
            long = "changelog",
            help = "Refresh CHANGELOG.md as part of this tag (requires a `changelog:` config block)"
        )]
        changelog: bool,
        /// `anodize tag rollback [...]` — failure-recovery counterpart.
        ///
        /// Subcommand is optional: bare `anodize tag` keeps its
        /// existing autotag behavior; only `anodize tag rollback`
        /// invokes the rollback flow.
        #[command(subcommand)]
        sub: Option<TagSub>,
    },
    /// Resume a release after a transient failure or after `--prepare`/`--split`
    ///
    /// With `--merge`: load every per-target `context.json` under `dist/` (one
    /// per split-build worker) and run the full post-build pipeline
    /// (sign / checksum / sbom / release / publish / announce).
    ///
    /// Without `--merge`: load existing `dist/` artifacts and run the
    /// publish-only pipeline (release / blob / publish). Use this to resume
    /// a single-host release that stalled during publish (e.g. expired
    /// token, transient 5xx) without rebuilding.
    ///
    /// `continue` vs `publish`: both consume a populated `dist/` and run
    /// the release / blob / publish chain. `continue` is the recommended
    /// alias for "resume a stalled single-host release" — the
    /// `continue` command and the in-repo `--prepare` → `continue`
    /// flow. `publish` is the lower-level entry point that does the same
    /// thing without the resume framing; prefer `continue` unless you're
    /// invoking the publish chain on a dist that was never paused. Neither
    /// is being deprecated.
    Continue {
        #[arg(
            long,
            help = "Merge artifacts from split build jobs and run post-build stages"
        )]
        merge: bool,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Skip stages or publishers (comma-separated, e.g. docker,announce,npm). \
                    Unified denylist: a stage name skips the stage, a publisher name \
                    (npm, homebrew, chocolatey, …) skips that publisher."
        )]
        skip: Vec<String>,
        #[arg(long = "publishers", value_delimiter = ',', help = PUBLISHERS_HELP_STEM)]
        publishers: Vec<String>,
        #[arg(
            long,
            help = TOKEN_HELP.as_str()
        )]
        token: Option<String>,
    },
    /// Run only the publish stages (release, blob, publish) from a completed dist/
    ///
    /// `publish` vs `continue`: both consume a populated `dist/` and run
    /// the same release / blob / publish chain. `publish` is the
    /// lower-level entry point — no resume framing, no after-hooks /
    /// milestone closure. `continue` is the recommended alias when
    /// resuming a stalled single-host release (the
    /// `continue` command); it additionally invokes the announce
    /// stage and treats the dist as a paused-release surface. Prefer
    /// `continue` unless you specifically want the unframed publish
    /// chain. `--dist` overrides the configured dist directory;
    /// `release` has no `--dist` because it produces dist.
    Publish {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(
            long,
            help = TOKEN_HELP.as_str()
        )]
        token: Option<String>,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
        #[arg(
            long,
            help = "Merge artifacts from `release --split` workers (dist/<subdir>/context.json) before running the publish-only pipeline. Mirrors `goreleaser publish --merge`."
        )]
        merge: bool,
        #[arg(
            long,
            help = "Force re-publish even when a prior report.json exists. \
                    WARNING: PR-based publishers will open duplicate pull requests."
        )]
        allow_rerun: bool,
        #[arg(
            long = "show-skipped",
            help = "Show per-crate 'no <publisher> config block' skip lines at default verbosity \
                    (normally only visible with --debug). Use to diagnose why a publisher didn't \
                    run for a given crate."
        )]
        show_skipped: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Skip stages or publishers (comma-separated, e.g. npm,blob). \
                    Unified denylist: a stage name skips the stage, a publisher name \
                    (npm, homebrew, chocolatey, …) skips that publisher."
        )]
        skip: Vec<String>,
        #[arg(long = "publishers", value_delimiter = ',', help = PUBLISHERS_HELP_STEM)]
        publishers: Vec<String>,
    },
    /// Bump crate versions (Conventional Commits → semver level)
    ///
    /// Infers the per-crate level from commits since each crate's last tag
    /// when no positional argument is given. `patch|minor|major`, an explicit
    /// version, or `release` (strip prerelease) are also accepted.
    Bump {
        #[arg(help = "patch | minor | major | <version> | release (omit to infer)")]
        level_or_version: Option<String>,
        #[arg(
            long,
            short = 'p',
            visible_alias = "crate",
            action = clap::ArgAction::Append,
            help = "Bump a specific crate (repeatable)"
        )]
        package: Vec<String>,
        #[arg(
            long,
            alias = "all",
            conflicts_with = "package",
            help = "Bump every workspace member (excluding publish=false)"
        )]
        workspace: bool,
        #[arg(
            long,
            action = clap::ArgAction::Append,
            help = "Exclude a crate from --workspace (repeatable)"
        )]
        exclude: Vec<String>,
        #[arg(long, help = "Append a prerelease identifier (e.g. rc.1)")]
        pre: Option<String>,
        #[arg(long, help = "Do not rewrite dependents' [dependencies] version specs")]
        exact: bool,
        #[arg(
            long,
            help = "Proceed even if the working tree has uncommitted changes"
        )]
        allow_dirty: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Print the plan without editing any files")]
        dry_run: bool,
        #[arg(long, help = "Stage edits and create a single commit")]
        commit: bool,
        #[arg(
            long = "changelog",
            requires = "commit",
            help = "Refresh CHANGELOG.md in the bump commit (requires --commit and a `changelog:` config block)"
        )]
        changelog: bool,
        #[arg(
            long,
            requires = "commit",
            help = "GPG-sign the commit (requires --commit)"
        )]
        sign: bool,
        #[arg(long, help = "Override the default commit message template")]
        commit_message: Option<String>,
        #[arg(
            long,
            default_value = "text",
            help = "Output format: text | json (json requires --dry-run)"
        )]
        output: String,
    },
    /// Run only the announce stage from a completed dist/
    ///
    /// Counterpart to `release --announce-only`: both re-fire announcers
    /// against a populated dist without re-publishing. The subcommand
    /// form (`anodizer announce`) accepts `--dist` to point at a
    /// non-default tree (e.g. preserved by `--preserve-dist`); the flag
    /// form (`release --announce-only`) operates on the dist configured
    /// in `.anodizer.yaml`. Both honor nightly short-circuit.
    Announce {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
        #[arg(
            long,
            help = TOKEN_HELP.as_str()
        )]
        token: Option<String>,
        #[arg(long, value_delimiter = ',', help = "Skip stages (comma-separated)")]
        skip: Vec<String>,
        #[arg(
            long,
            help = "Merge artifact lists from `release --split` workers (dist/<subdir>/context.json) before announcing. Mirrors `goreleaser announce --merge`."
        )]
        merge: bool,
    },
    /// Send a notification through configured announce integrations.
    ///
    /// Fires configured announce integrations (slack, discord, webhook, …) with
    /// a Tera-rendered message. Unlike `announce`, this command does not require
    /// a `dist/` directory — it is intended for ad-hoc notifications outside the
    /// release pipeline (e.g. CI status alerts, deployment notices).
    Notify {
        /// Message template to send. Supports standard Tera template vars
        /// (e.g. `{{ ProjectName }}`, `{{ Tag }}`, `{{ Version }}`).
        message: String,
        /// Comma-separated list of integration names to fire (default: all).
        /// Valid names: discord, discourse, slack, webhook, telegram, teams,
        /// mattermost, reddit, twitter, mastodon, bluesky, linkedin.
        #[arg(long = "publishers", value_delimiter = ',')]
        publishers: Vec<String>,
        /// Comma-separated list of integration names to omit.
        #[arg(long = "skip", value_delimiter = ',')]
        skip: Vec<String>,
        /// Send the message literally, without Tera template rendering. Use
        /// when the message contains untrusted text (e.g. error output in an
        /// on_error hook).
        #[arg(long)]
        raw: bool,
        /// Send secrets in the message body verbatim (disable outbound
        /// redaction). For trusted private channels only; log output stays
        /// redacted.
        #[arg(long = "allow-secrets")]
        allow_secrets: bool,
        /// Run without sending (dry-run mode).
        #[arg(long)]
        dry_run: bool,
    },
}

/// Output format for `anodizer changelog`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ChangelogFormat {
    /// Regenerate the `## [Unreleased]` section(s) of the configured
    /// `CHANGELOG.md` file(s) (the default). Previews to stdout; writes in
    /// place with `--write`.
    #[default]
    #[value(name = "keep-a-changelog", alias = "kac")]
    KeepAChangelog,
    /// GitHub-release-body markdown (grouped bullets) for the resolved range,
    /// to stdout. The historical `anodizer changelog` behavior.
    ReleaseNotes,
    /// Machine-readable JSON array of `{ crate, from, to, groups }` objects,
    /// one per selected crate, sorted by crate name.
    Json,
}

/// `anodize tag` parent subcommand.
///
/// Bare `anodize tag` keeps its existing autotag behavior (handled
/// by the `Tag` variant directly). `anodize tag rollback` opts into
/// the failure-recovery flow described in
/// [`commands::tag::rollback`].
#[derive(Subcommand)]
pub enum TagSub {
    /// Rollback anodize-managed tags at a SHA, then revert (or reset
    /// past) the bump commit they point at.
    Rollback {
        #[arg(
            value_name = "sha",
            help = "Commit SHA to roll back from. Defaults to HEAD."
        )]
        sha: Option<String>,
        #[arg(long, help = "Print what would happen without mutating anything")]
        dry_run: bool,
        #[arg(
            long = "no-push",
            help = "Skip remote tag delete and branch push (local-only)"
        )]
        no_push: bool,
        #[arg(
            long,
            help = "Override the published-state guard: roll back even when the tag's run summary shows a one-way-door publisher (crates.io, chocolatey, winget, snapcraft, ...) accepted the version, or — when no summary exists — when a published (non-draft) GitHub release exists for the tag. Without it, rollback refuses because those registries never accept the same version twice: the version is burned and the only clean recovery is fixing forward"
        )]
        force: bool,
        #[arg(
            long,
            default_value = "all",
            help = "Tag-shape filter: all | lockstep | per-crate"
        )]
        scope: String,
        #[arg(
            long,
            default_value = "revert",
            help = "Rollback strategy: revert (default; history-preserving) | reset (opt-in; rewrites history, requires --force-with-lease to push)"
        )]
        mode: String,
        #[arg(
            long,
            value_name = "name",
            help = "Branch name to push the revert commit to. Required when HEAD is detached and no local branch points at it (typical CI tag-push context, where GITHUB_REF_NAME is the tag — not the bump-commit branch). Pass --branch master (or whichever branch the bump commit was created on)."
        )]
        branch: Option<String>,
    },
}

/// `anodize check` parent subcommand.
///
/// `Config` is the historic `check` body (validate `.anodizer.yaml`); the
/// determinism harness is plumbed here so the flag set ships with this
/// commit, but the body lands in a follow-up task.
#[derive(Subcommand)]
pub enum CheckCmd {
    /// Validate the workspace's anodize config.
    Config {
        #[arg(long, help = "Validate a specific workspace in a monorepo config")]
        workspace: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Validate these skip tokens (stages or publishers) against the known set \
                    without running anything (comma-separated). Unified denylist: a stage name \
                    skips the stage, a publisher name (npm, homebrew, chocolatey, …) skips \
                    that publisher."
        )]
        skip: Vec<String>,
        #[arg(
            long = "publishers",
            value_delimiter = ',',
            help = concat!(
                "Validate-only: check that each name is a publisher the active config \
                 actually enables (a known but unconfigured publisher is rejected). ",
                "Comma-separated publishers to run (default: all configured). \
                 --skip always wins over --publishers.",
            )
        )]
        publishers: Vec<String>,
    },
    /// Run the determinism harness (build pipeline twice, diff artifacts).
    Determinism(CheckDeterminismArgs),
    /// Check that enrolled `version_files` still match each crate's current version.
    VersionFiles,
}

#[derive(clap::Args)]
pub struct CheckDeterminismArgs {
    #[arg(
        long,
        default_value = "2",
        help = "Number of from-clean rebuilds to diff"
    )]
    pub runs: u32,
    #[arg(
        long,
        value_name = "stages",
        help = "Optional stage subset (build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum,cargo-package,docker,msi,nsis,dmg,pkg,srpm,appbundle,appimage,flatpak, plus the `installers` family selector expanding to nfpm,makeself,srpm,msi,nsis,dmg,pkg). Omit the flag to byte-verify the full OS-native partition for this host (Linux adds nfpm/makeself/snapcraft/srpm/docker/appimage/flatpak; macOS adds appbundle/dmg/pkg; Windows adds msi/nsis). The list is also the build filter: stages NOT named here are added to the child release's `--skip=` set, so a stage must be requested (or in the host default) to be byte-verified. `cargo-package` is harness-only — drives `cargo package --no-verify --allow-dirty` per workspace member to probe `.crate` byte-stability without hitting a registry; it is NOT in the host default and stays opt-in. `docker` is harness-only — drives `docker buildx build --output=type=oci,rewrite-timestamp=true,dest=…` against each configured `dockers_v2` entry's rendered dockerfile (with its `extra_files` and `build_args`, mirroring the production `docker` stage) to probe OCI image byte-stability without pushing to a registry; skipped when `docker buildx` is unavailable or the crate configures no `dockers_v2`. Installer stages (msi/nsis/dmg/pkg/srpm) plus appimage (needs `linuxdeploy`) and flatpak (needs `flatpak-builder`) are skipped at the gate when their backing tool is absent — a host-default stage warn-skips, an explicitly typed one hard-fails; `appbundle` is pure file assembly and always runs when requested."
    )]
    pub stages: Option<String>,
    #[arg(
        long,
        value_name = "csv",
        help = "Restrict the harness to a comma-separated subset of configured target triples. Used by the sharded release workflow so each runner only validates targets it can natively build (Linux runner skips macOS targets, etc.). Forwarded to the child `anodize release --snapshot` subprocess."
    )]
    pub targets: Option<String>,
    #[arg(
        long,
        value_name = "path",
        help = "JSON report path; default dist/run-<id>/determinism.json"
    )]
    pub report: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "no_snapshot",
        help = "Force snapshot mode on the child release subprocess (artifacts get a `-SNAPSHOT-<sha>` suffix). Default: auto — snapshot off when HEAD is at a tag, on otherwise."
    )]
    pub snapshot: bool,
    #[arg(
        long = "no-snapshot",
        conflicts_with = "snapshot",
        help = "Force snapshot mode OFF on the child release subprocess (artifacts emit the actual release version). Default: auto — see --snapshot."
    )]
    pub no_snapshot: bool,
    #[arg(
        long = "inject-drift",
        value_name = "stage",
        hide = true,
        help = "(TEST HARNESS) Append 1 random byte to the first artifact emitted by <stage>. Gated by ANODIZE_TEST_HARNESS=1."
    )]
    pub inject_drift: Option<String>,
    #[arg(
        long = "preserve-dist",
        value_name = "path",
        help = "When the harness greens, copy run-0's `<worktree>/dist/**` to <path> and emit `<path>/context.json` describing the artifact set. The release workflow's publish-only path consumes this to ship the determinism step's output directly (eliminates the redundant `build:` recompilation). Local operators can pass this too — useful for inspecting a hermetic dist tree without re-running the release pipeline."
    )]
    pub preserve_dist: Option<PathBuf>,
    #[arg(
        long = "crate",
        value_name = "name",
        help = "When --preserve-dist is set, write the preserved dist tree to \
                <dest>/<name>/ instead of directly into <dest>/. Used by the \
                sharded matrix to produce per-crate subdirectories so a \
                `release --publish-only` job can merge all crates into a single \
                dist/ without context.json collision."
    )]
    pub crate_name: Option<String>,
    /// Fail (not warn-skip) if any selected stage's backing tool is missing —
    /// used by CI so a default host-OS run cannot silently skip an OS-native
    /// producer.
    ///
    /// Without `--stages`, the harness builds the full host-OS partition
    /// ([`crate::commands::check::determinism`]'s `default_stages_for_host`),
    /// and a host-default stage whose tool is absent normally warn-skips so dev
    /// boxes stay usable. CI provisions every OS-native tool and must treat a
    /// missing one as a hard failure: a silent skip is the exact false coverage
    /// that once hid the installer formats from every release. This flag
    /// promotes the WHOLE resolved stage set to the hard-fail contract that
    /// explicitly typed stages already get.
    #[arg(
        long = "require-tools",
        help = "Fail (not warn-skip) if any selected stage's backing tool is missing — used by CI so a default host-OS run cannot silently skip an OS-native producer."
    )]
    pub require_tools: bool,
}

/// Clap `value_parser` for `--from-run=<id>`.
///
/// `run_id` is operator-controlled and is joined directly into a
/// filesystem path (`<dist>/run-<id>/{report,rollback}.json`) by the
/// `--rollback-only` replay code. Without this validator,
/// `--from-run=../../etc/passwd` would resolve to a traversed path on
/// both read (`report.json`) and write (`rollback.json`) — operator
/// data-loss potential.
///
/// Delegates to [`anodizer_stage_publish::rollback_only::validate_run_id`]
/// so the rule has a single source of truth (the same validator runs at
/// the `run_with_publishers` entry point as a defense-in-depth guard).
fn parse_run_id(s: &str) -> Result<String, String> {
    anodizer_stage_publish::rollback_only::validate_run_id(s)
        .map(|()| s.to_string())
        .map_err(|err| format!("{:#}", err))
}

/// Detect the host target triple by parsing `rustc -vV` output.
/// Delegates to `anodizer_core::partial::detect_host_target()`.
pub fn detect_host_target() -> anyhow::Result<String> {
    anodizer_core::partial::detect_host_target()
}

/// Return a sensible default parallelism value (number of logical CPUs, minimum 1).
pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Build the clap `Command` tree for CLI introspection.
pub fn build_cli() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}
