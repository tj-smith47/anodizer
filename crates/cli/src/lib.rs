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
        #[arg(long, help = "GitHub token (overrides GITHUB_TOKEN env var)")]
        token: Option<String>,
        #[arg(
            long,
            default_value = "30m",
            help = "Pipeline timeout duration (e.g., 30m, 1h, 5s)"
        )]
        timeout: String,
        #[arg(long, short = 'p', default_value_t = num_cpus(), help = "Maximum number of parallel build jobs")]
        parallelism: usize,
        #[arg(long, help = "Automatically set --snapshot if the git repo is dirty")]
        auto_snapshot: bool,
        #[arg(long, help = "Build only for the host target triple")]
        single_target: bool,
        #[arg(
            long,
            help = "Path to a custom release notes file (overrides changelog)"
        )]
        release_notes: Option<PathBuf>,
        #[arg(long, help = "Release a specific workspace in a monorepo config")]
        workspace: Option<String>,
        #[arg(long, help = "Set the release as a draft")]
        draft: bool,
        #[arg(long, help = "Path to a file containing custom release header text")]
        release_header: Option<PathBuf>,
        #[arg(long, help = "Path to a file containing custom release footer text")]
        release_footer: Option<PathBuf>,
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
            default_value = "30m",
            help = "Pipeline timeout duration (e.g., 30m, 1h, 5s)"
        )]
        timeout: String,
        #[arg(long, short = 'p', default_value_t = num_cpus(), help = "Maximum number of parallel build jobs")]
        parallelism: usize,
        #[arg(long, help = "Build only for the host target triple")]
        single_target: bool,
        #[arg(long, help = "Build a specific workspace in a monorepo config")]
        workspace: Option<String>,
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
    /// Output JSON Schema for .anodize.yaml
    Jsonschema,
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
}

/// Detect the host target triple by parsing `rustc -vV` output.
pub fn detect_host_target() -> anyhow::Result<String> {
    let output = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run rustc: {}", e))?;
    if !output.status.success() {
        anyhow::bail!("rustc -vV failed");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(triple) = line.strip_prefix("host: ") {
            return Ok(triple.trim().to_string());
        }
    }
    anyhow::bail!("could not find 'host:' line in rustc -vV output")
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
