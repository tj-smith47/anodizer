use clap::{Parser, Subcommand};
use clap_complete::Shell;
use colored::Colorize;
use std::path::PathBuf;

mod commands;
mod pipeline;
pub mod timeout;

#[derive(Parser)]
#[command(name = "anodize", version, about = "Release Rust projects with ease")]
pub struct Cli {
    #[arg(
        long,
        short = 'f',
        global = true,
        help = "Path to config file (overrides auto-detection)"
    )]
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
    /// Generate shell completions
    Completion {
        #[arg(value_enum, help = "Shell to generate completions for")]
        shell: Shell,
    },
    /// Check availability of required external tools
    Healthcheck,
}

/// Return a sensible default parallelism value (number of logical CPUs, minimum 1).
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Detect the host target triple by parsing `rustc -vV` output.
fn detect_host_target() -> anyhow::Result<String> {
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

/// Check if the git working tree is dirty using `git status --porcelain`.
fn is_git_dirty() -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Resolve --single-target flag to the actual host target triple.
fn resolve_single_target(single_target: bool) -> Option<String> {
    if single_target {
        match detect_host_target() {
            Ok(triple) => {
                eprintln!(
                    "{} building only for host target: {}",
                    "Note:".cyan().bold(),
                    triple
                );
                Some(triple)
            }
            Err(e) => {
                eprintln!(
                    "{} failed to detect host target: {}",
                    "Error:".red().bold(),
                    e
                );
                std::process::exit(1);
            }
        }
    } else {
        None
    }
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Release {
            crate_names,
            all,
            force,
            snapshot,
            dry_run,
            clean,
            skip,
            token,
            timeout,
            parallelism,
            auto_snapshot,
            single_target,
            release_notes,
        } => {
            let duration = timeout::parse_duration(&timeout).unwrap_or_else(|e| {
                eprintln!(
                    "{} invalid --timeout value '{}': {}",
                    "Error:".red().bold(),
                    timeout,
                    e
                );
                std::process::exit(1);
            });

            // Resolve --auto-snapshot: if set and repo is dirty, force snapshot mode
            let effective_snapshot = if !snapshot && auto_snapshot && is_git_dirty() {
                eprintln!(
                    "{} repo is dirty, automatically enabling snapshot mode",
                    "Note:".yellow().bold()
                );
                true
            } else {
                snapshot
            };

            let resolved_single_target = resolve_single_target(single_target);

            timeout::run_with_timeout(duration, || {
                commands::release::run(commands::release::ReleaseOpts {
                    crate_names,
                    all,
                    force,
                    snapshot: effective_snapshot,
                    dry_run,
                    clean,
                    skip,
                    token,
                    verbose: cli.verbose,
                    debug: cli.debug,
                    config_override: cli.config.clone(),
                    parallelism,
                    single_target: resolved_single_target,
                    release_notes,
                })
            })
        }
        Commands::Build {
            crate_names,
            timeout,
            parallelism,
            single_target,
        } => {
            let duration = timeout::parse_duration(&timeout).unwrap_or_else(|e| {
                eprintln!(
                    "{} invalid --timeout value '{}': {}",
                    "Error:".red().bold(),
                    timeout,
                    e
                );
                std::process::exit(1);
            });
            let config_override = cli.config.clone();
            let resolved_single_target = resolve_single_target(single_target);

            timeout::run_with_timeout(duration, move || {
                commands::build::run(commands::build::BuildOpts {
                    crate_names,
                    config_override,
                    parallelism,
                    single_target: resolved_single_target,
                })
            })
        }
        Commands::Check => commands::check::run(cli.config.as_deref()),
        Commands::Init => commands::init::run(),
        Commands::Changelog { crate_name } => {
            commands::changelog::run(crate_name, cli.config.as_deref())
        }
        Commands::Completion { shell } => commands::completion::run(shell),
        Commands::Healthcheck => commands::healthcheck::run(),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses_release_with_new_flags() {
        let cli = Cli::try_parse_from([
            "anodize",
            "release",
            "--parallelism",
            "8",
            "--auto-snapshot",
            "--single-target",
            "--release-notes",
            "/tmp/notes.md",
        ]);
        assert!(
            cli.is_ok(),
            "CLI should parse release with new flags: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_release_parallelism_short() {
        let cli = Cli::try_parse_from(["anodize", "release", "-p", "2"]);
        assert!(
            cli.is_ok(),
            "CLI should parse -p shorthand: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_build_with_new_flags() {
        let cli =
            Cli::try_parse_from(["anodize", "build", "--parallelism", "4", "--single-target"]);
        assert!(
            cli.is_ok(),
            "CLI should parse build with new flags: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_completion() {
        let cli = Cli::try_parse_from(["anodize", "completion", "bash"]);
        assert!(
            cli.is_ok(),
            "CLI should parse completion command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_healthcheck() {
        let cli = Cli::try_parse_from(["anodize", "healthcheck"]);
        assert!(
            cli.is_ok(),
            "CLI should parse healthcheck command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_release_default_parallelism() {
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        if let Commands::Release { parallelism, .. } = cli.command {
            assert!(
                parallelism >= 1,
                "default parallelism should be at least 1, got {}",
                parallelism
            );
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_build_default_parallelism() {
        let cli = Cli::try_parse_from(["anodize", "build"]).unwrap();
        if let Commands::Build { parallelism, .. } = cli.command {
            assert!(
                parallelism >= 1,
                "default parallelism should be at least 1, got {}",
                parallelism
            );
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_num_cpus_returns_positive() {
        assert!(num_cpus() >= 1, "num_cpus should return at least 1");
    }

    #[test]
    fn test_detect_host_target_returns_triple() {
        let result = detect_host_target();
        assert!(
            result.is_ok(),
            "detect_host_target should succeed: {:?}",
            result.err()
        );
        let triple = result.unwrap();
        assert!(!triple.is_empty(), "host target triple should not be empty");
        // A target triple should contain at least two dashes (e.g., x86_64-unknown-linux-gnu)
        assert!(
            triple.contains('-'),
            "host target triple should contain dashes: {}",
            triple
        );
    }

    #[test]
    fn test_completion_shells_are_accepted() {
        for shell in ["bash", "zsh", "fish", "powershell"] {
            let cli = Cli::try_parse_from(["anodize", "completion", shell]);
            assert!(
                cli.is_ok(),
                "CLI should accept completion for {}: {:?}",
                shell,
                cli.err()
            );
        }
    }

    #[test]
    fn test_help_output_contains_new_commands() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("completion"),
            "help should mention completion command"
        );
        assert!(
            help.contains("healthcheck"),
            "help should mention healthcheck command"
        );
    }
}
