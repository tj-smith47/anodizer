use clap::{Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "anodize", version, about = "Release Rust projects with ease")]
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
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the full release pipeline
    Release {
        #[arg(long = "crate", action = clap::ArgAction::Append, help = "Release a specific crate (repeatable)")]
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
            help = "GitHub token (overrides ANODIZE_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
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
            help = "Path to a custom release notes file (overrides changelog)"
        )]
        release_notes: Option<PathBuf>,
        #[arg(
            long,
            conflicts_with = "crate_names",
            help = "Release a specific workspace in a monorepo config"
        )]
        workspace: Option<String>,
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
    /// Validate configuration
    Check {
        #[arg(long, help = "Validate a specific workspace in a monorepo config")]
        workspace: Option<String>,
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
    /// Output JSON Schema for .anodize.yaml
    Jsonschema,
    /// Resolve a git tag to its matching crate in the config
    ResolveTag {
        #[arg(help = "Tag to resolve (e.g. 'v1.2.3', 'core-v0.2.3')")]
        tag: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
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
    /// Continue a split release by merging artifacts and running post-build stages
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
            help = "GitHub token (overrides ANODIZE_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
    },
    /// Run only the publish stages (release, publish, blob) from a completed dist/
    Publish {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZE_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
    },
    /// Run only the announce stage from a completed dist/
    Announce {
        #[arg(long, help = "Run full pipeline without side effects")]
        dry_run: bool,
        #[arg(long, help = "Custom dist directory (overrides config)")]
        dist: Option<PathBuf>,
        #[arg(
            long,
            help = "GitHub token (overrides ANODIZE_GITHUB_TOKEN / GITHUB_TOKEN env vars)"
        )]
        token: Option<String>,
        #[arg(long, value_delimiter = ',', help = "Skip stages (comma-separated)")]
        skip: Vec<String>,
    },
}

/// Detect the host target triple by parsing `rustc -vV` output.
/// Delegates to `anodize_core::partial::detect_host_target()`.
pub fn detect_host_target() -> anyhow::Result<String> {
    anodize_core::partial::detect_host_target()
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
