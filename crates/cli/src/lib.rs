use clap::{Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

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
            help = "Skip stages (comma-separated, e.g. docker,announce)"
        )]
        skip: Vec<String>,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
        #[arg(
            long,
            default_value = "60m",
            help = "Pipeline timeout duration (e.g., 60m, 1h, 5s)"
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
            long = "allow-nondeterministic",
            value_name = "name=reason",
            action = clap::ArgAction::Append,
            help = "Runtime non-determinism opt-out for a specific artifact (repeatable). Mutually exclusive with --strict."
        )]
        allow_nondeterministic: Vec<String>,
        #[arg(
            long = "summary-json",
            value_name = "path",
            help = "Write the per-publisher run summary JSON to this path."
        )]
        summary_json: Option<PathBuf>,
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
            conflicts_with_all = ["split", "merge"],
            help = "Load artifacts from dist/ (preserved by `anodize check determinism --preserve-dist`) and run only the sign + publish pipeline. Skips build/archive/nfpm/sbom/checksum — those stages' outputs must already be present in dist/."
        )]
        publish_only: bool,
        #[arg(
            long,
            help = "Run local build + archive + sign + checksum + sbom stages but skip release / publish / announce (GoReleaser Pro parity). Artifacts stay in dist/ for inspection."
        )]
        prepare: bool,
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
    },
    /// Build binaries only (always runs in snapshot mode)
    Build {
        #[arg(long = "crate", action = clap::ArgAction::Append, help = "Build a specific crate (repeatable)")]
        crate_names: Vec<String>,
        #[arg(
            long,
            default_value = "60m",
            help = "Pipeline timeout duration (e.g., 60m, 1h, 5s)"
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
    /// Generate starter config
    Init,
    /// Generate changelog only
    Changelog {
        #[arg(long = "crate", help = "Generate changelog for a specific crate")]
        crate_name: Option<String>,
    },
    /// Generate shell completions
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    /// Check availability of required external tools
    Healthcheck,
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
    /// Auto-tag based on commit message directives
    Tag {
        #[arg(long, help = "Show what tag would be created without pushing")]
        dry_run: bool,
        #[arg(long, help = "Override bump logic with a specific tag value")]
        custom_tag: Option<String>,
        #[arg(long, help = "Override default bump type (patch/minor/major)")]
        default_bump: Option<String>,
        #[arg(long = "crate", help = "Tag a specific crate in a workspace")]
        crate_name: Option<String>,
    },
    /// Resume a release after a transient failure or after `--prepare`/`--split`
    ///
    /// With `--merge`: load every per-target `context.json` under `dist/` (one
    /// per split-build worker) and run the full post-build pipeline
    /// (sign / checksum / sbom / release / publish / announce).
    ///
    /// Without `--merge`: load existing `dist/` artifacts and run the
    /// publish-only pipeline (release / publish / blob). Use this to resume
    /// a single-host release that stalled during publish (e.g. expired
    /// token, transient 5xx) without rebuilding.
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
            help = "Skip stages (comma-separated, e.g. docker,announce)"
        )]
        skip: Vec<String>,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
    },
    /// Run only the publish stages (release, publish, blob) from a completed dist/
    Publish {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
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
    Announce {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
        #[arg(long, value_delimiter = ',', help = "Skip stages (comma-separated)")]
        skip: Vec<String>,
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
    },
    /// Run the determinism harness (build pipeline twice, diff artifacts).
    Determinism(CheckDeterminismArgs),
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
        help = "Optional stage subset (build,archive,sbom,sign,checksum,cargo-package). `cargo-package` is harness-only — drives `cargo package --no-verify --allow-dirty` per workspace member to probe `.crate` byte-stability without hitting a registry."
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
