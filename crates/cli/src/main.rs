use anodize_cli::{Cli, Commands, detect_host_target};

use clap::Parser;
use colored::Colorize;

mod commands;
mod pipeline;
pub mod timeout;

/// Parse a --timeout value or exit with an error message.
fn parse_timeout_or_exit(timeout: &str) -> std::time::Duration {
    timeout::parse_duration(timeout).unwrap_or_else(|e| {
        eprintln!(
            "{} invalid --timeout value '{}': {}",
            "Error:".red().bold(),
            timeout,
            e
        );
        std::process::exit(1);
    })
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
            nightly,
            dry_run,
            clean,
            skip,
            token,
            timeout,
            parallelism,
            auto_snapshot,
            single_target,
            release_notes,
            workspace,
            draft,
            release_header,
            release_footer,
            split,
            merge,
        } => {
            let duration = parse_timeout_or_exit(&timeout);

            // Resolve --auto-snapshot: if set and repo is dirty, force snapshot mode
            let effective_snapshot =
                if !snapshot && auto_snapshot && anodize_core::git::is_git_dirty() {
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
                    nightly,
                    dry_run,
                    clean,
                    skip,
                    token,
                    verbose: cli.verbose,
                    debug: cli.debug,
                    quiet: cli.quiet,
                    config_override: cli.config.clone(),
                    parallelism,
                    single_target: resolved_single_target,
                    release_notes,
                    workspace,
                    draft,
                    release_header,
                    release_footer,
                    split,
                    merge,
                })
            })
        }
        Commands::Build {
            crate_names,
            timeout,
            parallelism,
            single_target,
            workspace,
        } => {
            let duration = parse_timeout_or_exit(&timeout);
            let config_override = cli.config.clone();
            let resolved_single_target = resolve_single_target(single_target);
            let verbose = cli.verbose;
            let debug = cli.debug;
            let quiet = cli.quiet;

            timeout::run_with_timeout(duration, move || {
                commands::build::run(commands::build::BuildOpts {
                    crate_names,
                    config_override,
                    verbose,
                    debug,
                    quiet,
                    parallelism,
                    single_target: resolved_single_target,
                    workspace,
                })
            })
        }
        Commands::Check { workspace } => commands::check::run(
            cli.config.as_deref(),
            workspace.as_deref(),
            cli.verbose,
            cli.debug,
            cli.quiet,
        ),
        Commands::Init => commands::init::run(),
        Commands::Changelog { crate_name } => commands::changelog::run(
            crate_name,
            cli.config.as_deref(),
            cli.verbose,
            cli.debug,
            cli.quiet,
        ),
        Commands::Completion { shell } => commands::completion::run(shell),
        Commands::Healthcheck => commands::healthcheck::run(),
        Commands::Jsonschema => commands::jsonschema::run(),
        Commands::Tag {
            dry_run,
            custom_tag,
            default_bump,
            crate_name,
        } => commands::tag::run(commands::tag::TagOpts {
            dry_run,
            custom_tag,
            default_bump,
            crate_name,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
        }),
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
    use anodize_cli::num_cpus;
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
        assert!(help.contains("tag"), "help should mention tag command");
        assert!(
            help.contains("jsonschema"),
            "help should mention jsonschema command"
        );
    }

    #[test]
    fn test_cli_parses_jsonschema() {
        let cli = Cli::try_parse_from(["anodize", "jsonschema"]);
        assert!(
            cli.is_ok(),
            "CLI should parse jsonschema command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_tag_dry_run() {
        let cli = Cli::try_parse_from(["anodize", "tag", "--dry-run"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --dry-run: {:?}",
            cli.err()
        );
        if let Commands::Tag { dry_run, .. } = cli.unwrap().command {
            assert!(dry_run);
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_custom_tag() {
        let cli = Cli::try_parse_from(["anodize", "tag", "--custom-tag", "v5.0.0"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --custom-tag: {:?}",
            cli.err()
        );
        if let Commands::Tag { custom_tag, .. } = cli.unwrap().command {
            assert_eq!(custom_tag, Some("v5.0.0".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_default_bump() {
        let cli = Cli::try_parse_from(["anodize", "tag", "--default-bump", "major"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --default-bump: {:?}",
            cli.err()
        );
        if let Commands::Tag { default_bump, .. } = cli.unwrap().command {
            assert_eq!(default_bump, Some("major".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_crate_flag() {
        let cli = Cli::try_parse_from(["anodize", "tag", "--crate", "my-lib"]);
        assert!(cli.is_ok(), "CLI should parse tag --crate: {:?}", cli.err());
        if let Commands::Tag { crate_name, .. } = cli.unwrap().command {
            assert_eq!(crate_name, Some("my-lib".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_all_flags() {
        let cli = Cli::try_parse_from([
            "anodize",
            "tag",
            "--dry-run",
            "--custom-tag",
            "v2.0.0",
            "--default-bump",
            "patch",
            "--crate",
            "core",
        ]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag with all flags: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_release_nightly_flag() {
        let cli = Cli::try_parse_from(["anodize", "release", "--nightly"]);
        assert!(
            cli.is_ok(),
            "CLI should parse release --nightly: {:?}",
            cli.err()
        );
        if let Commands::Release { nightly, .. } = cli.unwrap().command {
            assert!(nightly, "--nightly flag should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_nightly_defaults_false() {
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        if let Commands::Release { nightly, .. } = cli.command {
            assert!(!nightly, "--nightly should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_help_output_contains_nightly_flag() {
        let mut cmd = Cli::command();
        // Check the release subcommand help for --nightly
        let release_help = cmd
            .find_subcommand_mut("release")
            .expect("release subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            release_help.contains("--nightly"),
            "release help should mention --nightly flag, got: {}",
            release_help
        );
    }

    #[test]
    fn test_cli_parses_release_workspace_flag() {
        let cli = Cli::try_parse_from(["anodize", "release", "--workspace", "frontend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse release --workspace: {:?}",
            cli.err()
        );
        if let Commands::Release { workspace, .. } = cli.unwrap().command {
            assert_eq!(workspace, Some("frontend".to_string()));
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_release_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        if let Commands::Release { workspace, .. } = cli.command {
            assert!(workspace.is_none(), "--workspace should default to None");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_help_output_contains_workspace_flag() {
        let mut cmd = Cli::command();
        let release_help = cmd
            .find_subcommand_mut("release")
            .expect("release subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            release_help.contains("--workspace"),
            "release help should mention --workspace flag, got: {}",
            release_help
        );
    }

    // ---- Build --workspace tests ----

    #[test]
    fn test_cli_parses_build_workspace_flag() {
        let cli = Cli::try_parse_from(["anodize", "build", "--workspace", "frontend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse build --workspace: {:?}",
            cli.err()
        );
        if let Commands::Build { workspace, .. } = cli.unwrap().command {
            assert_eq!(workspace, Some("frontend".to_string()));
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_cli_build_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodize", "build"]).unwrap();
        if let Commands::Build { workspace, .. } = cli.command {
            assert!(
                workspace.is_none(),
                "build --workspace should default to None"
            );
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_help_output_build_contains_workspace_flag() {
        let mut cmd = Cli::command();
        let build_help = cmd
            .find_subcommand_mut("build")
            .expect("build subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            build_help.contains("--workspace"),
            "build help should mention --workspace flag, got: {}",
            build_help
        );
    }

    // ---- Check --workspace tests ----

    #[test]
    fn test_cli_parses_check_workspace_flag() {
        let cli = Cli::try_parse_from(["anodize", "check", "--workspace", "backend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse check --workspace: {:?}",
            cli.err()
        );
        if let Commands::Check { workspace } = cli.unwrap().command {
            assert_eq!(workspace, Some("backend".to_string()));
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn test_cli_check_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodize", "check"]).unwrap();
        if let Commands::Check { workspace } = cli.command {
            assert!(
                workspace.is_none(),
                "check --workspace should default to None"
            );
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn test_help_output_check_contains_workspace_flag() {
        let mut cmd = Cli::command();
        let check_help = cmd
            .find_subcommand_mut("check")
            .expect("check subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            check_help.contains("--workspace"),
            "check help should mention --workspace flag, got: {}",
            check_help
        );
    }

    #[test]
    fn test_cli_parses_quiet_flag() {
        // --quiet long form
        let cli = Cli::try_parse_from(["anodize", "--quiet", "release"]);
        assert!(cli.is_ok(), "CLI should parse --quiet: {:?}", cli.err());
        assert!(cli.unwrap().quiet, "--quiet should set quiet to true");

        // -q short form
        let cli = Cli::try_parse_from(["anodize", "-q", "release"]);
        assert!(cli.is_ok(), "CLI should parse -q: {:?}", cli.err());
        assert!(cli.unwrap().quiet, "-q should set quiet to true");

        // quiet defaults to false
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        assert!(!cli.quiet, "quiet should default to false");
    }

    #[test]
    fn test_cli_parses_release_draft_flag() {
        let cli = Cli::try_parse_from(["anodize", "release", "--draft"]);
        assert!(cli.is_ok(), "CLI should parse --draft: {:?}", cli.err());
        if let Commands::Release { draft, .. } = cli.unwrap().command {
            assert!(draft, "--draft should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_draft_defaults_false() {
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        if let Commands::Release { draft, .. } = cli.command {
            assert!(!draft, "--draft should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_release_header_footer() {
        let cli = Cli::try_parse_from([
            "anodize",
            "release",
            "--release-header",
            "/tmp/header.md",
            "--release-footer",
            "/tmp/footer.md",
        ]);
        assert!(
            cli.is_ok(),
            "CLI should parse --release-header/--release-footer: {:?}",
            cli.err()
        );
        if let Commands::Release {
            release_header,
            release_footer,
            ..
        } = cli.unwrap().command
        {
            assert_eq!(
                release_header,
                Some(std::path::PathBuf::from("/tmp/header.md"))
            );
            assert_eq!(
                release_footer,
                Some(std::path::PathBuf::from("/tmp/footer.md"))
            );
        } else {
            panic!("expected Release command");
        }
    }

    // ---- Split/merge CLI flag tests ----

    #[test]
    fn test_cli_parses_release_split_flag() {
        let cli = Cli::try_parse_from(["anodize", "release", "--split"]);
        assert!(cli.is_ok(), "CLI should parse --split: {:?}", cli.err());
        if let Commands::Release { split, merge, .. } = cli.unwrap().command {
            assert!(split, "--split should be true");
            assert!(!merge, "--merge should be false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_release_merge_flag() {
        let cli = Cli::try_parse_from(["anodize", "release", "--merge"]);
        assert!(cli.is_ok(), "CLI should parse --merge: {:?}", cli.err());
        if let Commands::Release { split, merge, .. } = cli.unwrap().command {
            assert!(!split, "--split should be false");
            assert!(merge, "--merge should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_split_merge_default_false() {
        let cli = Cli::try_parse_from(["anodize", "release"]).unwrap();
        if let Commands::Release { split, merge, .. } = cli.command {
            assert!(!split, "--split should default to false");
            assert!(!merge, "--merge should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_split_with_single_target() {
        let cli = Cli::try_parse_from(["anodize", "release", "--split", "--single-target"]);
        assert!(
            cli.is_ok(),
            "CLI should parse --split --single-target: {:?}",
            cli.err()
        );
        if let Commands::Release {
            split,
            single_target,
            ..
        } = cli.unwrap().command
        {
            assert!(split);
            assert!(single_target);
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_help_output_contains_split_merge_flags() {
        let mut cmd = Cli::command();
        let release_help = cmd
            .find_subcommand_mut("release")
            .expect("release subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            release_help.contains("--split"),
            "release help should mention --split flag, got: {}",
            release_help
        );
        assert!(
            release_help.contains("--merge"),
            "release help should mention --merge flag, got: {}",
            release_help
        );
    }

    #[test]
    fn test_cli_split_merge_mutually_exclusive() {
        let result = Cli::try_parse_from(["anodize", "release", "--split", "--merge"]);
        assert!(
            result.is_err(),
            "--split and --merge should be mutually exclusive"
        );
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(
            err.contains("--split") || err.contains("--merge") || err.contains("cannot be used"),
            "error should mention the conflicting flags: {}",
            err
        );
    }
}
