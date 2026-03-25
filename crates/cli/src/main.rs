use std::path::PathBuf;
use clap::{Parser, Subcommand};
use colored::Colorize;

mod commands;
mod pipeline;

#[derive(Parser)]
#[command(name = "anodize", version, about = "Release Rust projects with ease")]
struct Cli {
    #[arg(long, short = 'f', global = true, help = "Path to config file (overrides auto-detection)")]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Build binaries only
    Build {
        #[arg(long = "crate", action = clap::ArgAction::Append)]
        crate_names: Vec<String>,
    },
    /// Validate configuration
    Check,
    /// Generate starter config
    Init,
    /// Generate changelog only
    Changelog {
        #[arg(long = "crate")]
        crate_name: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let config_path = cli.config.as_deref();
    let result = match cli.command {
        Commands::Release { crate_names, all, force, snapshot, dry_run, clean, skip, token } => {
            commands::release::run(commands::release::ReleaseOpts {
                crate_names, all, force, snapshot, dry_run, clean, skip, token,
                verbose: cli.verbose, debug: cli.debug,
                config_override: cli.config.clone(),
            })
        }
        Commands::Build { crate_names } => commands::build::run(crate_names, config_path),
        Commands::Check => commands::check::run(config_path),
        Commands::Init => commands::init::run(),
        Commands::Changelog { crate_name } => commands::changelog::run(crate_name, config_path),
    };
    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        // Print the error chain
        for cause in e.chain().skip(1) {
            eprintln!("  {} {}", "caused by:".dimmed(), cause);
        }
        std::process::exit(1);
    }
}
