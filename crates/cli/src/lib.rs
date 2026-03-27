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
    #[arg(long, global = true)]
    pub verbose: bool,
    #[arg(long, global = true)]
    pub debug: bool,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the full release pipeline
    Release {
        #[arg(long = "crate", action = clap::ArgAction::Append)]
        crate_names: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        snapshot: bool,
        #[arg(long, help = "Create a nightly release with date-based version")]
        nightly: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        #[arg(long)]
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
    },
    /// Build binaries only
    Build {
        #[arg(long = "crate", action = clap::ArgAction::Append)]
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
        #[arg(long = "crate")]
        crate_name: Option<String>,
    },
    /// Generate shell completions
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    /// Check availability of required external tools
    Healthcheck,
    /// Auto-tag based on commit message directives
    Tag {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, help = "Override bump logic with a specific tag value")]
        custom_tag: Option<String>,
        #[arg(long, help = "Override default bump type (patch/minor/major)")]
        default_bump: Option<String>,
        #[arg(long = "crate", help = "Tag a specific crate in a workspace")]
        crate_name: Option<String>,
    },
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
