use anodizer_cli::{CheckCmd, Cli, Commands, TagSub, num_cpus};
use anodizer_core::context::{VALID_BUILD_SKIPS, VALID_RELEASE_SKIPS, validate_skip_values};

use clap::{CommandFactory, FromArgMatches};
use colored::Colorize;

mod commands;
mod determinism_harness;
mod pipeline;
pub mod timeout;

/// Parse a --timeout value or exit with an error message.
fn parse_timeout_or_exit(timeout: &str) -> std::time::Duration {
    timeout::parse_duration(timeout).unwrap_or_else(|e| {
        eprintln!(
            "{}",
            anodizer_core::log::render_error(&format!("invalid --timeout value '{timeout}': {e}"))
        );
        std::process::exit(1);
    })
}

/// Parse a comma-separated triple list (`--targets=...`) into the
/// canonical `Option<Vec<String>>` form consumed by `ReleaseOpts.targets`
/// and the Determinism Harness dispatcher.
///
/// Thin wrapper over `commands::helpers::parse_csv_list` that supplies
/// the `--targets`-shaped error hint. See that function for the full
/// trim / drop-empty / err-on-all-empty matrix.
fn parse_targets_csv(raw: Option<&str>) -> Result<Option<Vec<String>>, String> {
    commands::helpers::parse_csv_list(
        raw,
        "--targets=x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu",
    )
}

/// Resolve --single-target flag to the actual host target triple.
///
/// Honours GoReleaser's priority chain
/// (see `anodizer_core::partial::resolve_host_target_with_env`):
/// `TARGET=<triple>` > `GGOOS`/`GGOARCH` host-rewrite > `rustc -vV`.
/// CI jobs targeting a non-host triple can therefore drive
/// `--single-target` with `TARGET=x86_64-unknown-linux-musl` without
/// changing the runner's actual architecture, matching the GR escape
/// hatch documented under `goreleaser build --single-target`.
///
/// The resolved triple is also exported as `TARGET=<triple>` to the
/// process environment so any downstream hook subprocess (`hooks.before`,
/// per-build `pre`/`post`) inherits the active target — parity with
/// GR's `partial.Pipe.Run` populating `ctx.PartialTarget` for the rest
/// of the pipeline.
fn resolve_single_target(single_target: bool) -> Option<String> {
    if !single_target {
        return None;
    }
    match anodizer_core::partial::resolve_host_target() {
        Ok(triple) => {
            eprintln!(
                "{}",
                anodizer_core::log::render_note(&format!(
                    "building only for host target: {triple}"
                ))
            );
            // SAFETY: single-threaded CLI startup, well before any
            // worker threads or pipeline workers spawn. Setting `TARGET`
            // here is required so user hooks see the resolved triple,
            // matching GoReleaser's `cmd/build.go` behaviour.
            unsafe {
                std::env::set_var("TARGET", &triple);
            }
            Some(triple)
        }
        Err(e) => {
            eprintln!(
                "{}",
                anodizer_core::log::render_error(&format!("failed to detect host target: {e}"))
            );
            std::process::exit(1);
        }
    }
}

/// Enable ANSI color output in non-TTY CI environments that still render
/// color in logs (GitHub Actions, GitLab, CircleCI, most modern systems).
///
/// The `colored` crate auto-disables when stderr is not a real TTY, which
/// means every CI run would show plain text. GitHub Actions, like cargo,
/// preserves ANSI escapes in the log stream and renders them in the web
/// UI, so the right behaviour is "force color when a CI environment is
/// detected, unless the user has opted out via NO_COLOR".
fn enable_ci_colors() {
    // Honour the user's explicit opt-out first.
    if std::env::var_os("NO_COLOR").is_some() {
        return;
    }
    // Respect explicit overrides — ANODIZER_COLOR or CLICOLOR_FORCE.
    if let Ok(val) = std::env::var("ANODIZER_COLOR") {
        match val.as_str() {
            "always" => {
                colored::control::set_override(true);
                return;
            }
            "never" => {
                colored::control::set_override(false);
                return;
            }
            _ => {}
        }
    }
    // Auto-enable in common CI environments.
    let ci_envs = ["GITHUB_ACTIONS", "GITLAB_CI", "CIRCLECI", "BUILDKITE", "CI"];
    for key in ci_envs {
        if std::env::var_os(key).is_some() {
            colored::control::set_override(true);
            return;
        }
    }
}

/// `tracing` event formatter that renders library-side `warn!`/`error!`
/// in the exact visual shape of [`anodizer_core::log::StageLogger`] — a
/// bold `Warning:` / `Error:` prefix followed by the event's message —
/// with NO ` WARN ` ansi level badge and NO `key=value` field/target
/// clutter. This is the single output authority: the few user-facing
/// warnings emitted from pure-library code paths (config validation,
/// defaults merge) that have no `StageLogger` in scope therefore look
/// identical to logger output instead of the default subscriber's
/// `2026-… WARN target: msg key=value` line, which previously
/// interleaved a third warning shape into release stderr.
struct StageStyleFormat;

/// Field visitor that captures only the `message` field of a tracing
/// event, discarding all structured `key=value` fields so they never
/// reach the rendered line.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // Only the synthetic `message` field is rendered; every structured
        // `key=value` field is dropped here so it never reaches the line.
        // The `message` arrives as `tracing`'s `format_args!` result, whose
        // `Debug` impl is identical to its `Display` (no surrounding
        // quotes) — so a string message renders cleanly. Any stray quoting
        // (e.g. a future caller passing `?msg`) is stripped defensively so
        // the user never sees a `"…"`-wrapped warning.
        if field.name() == "message" {
            self.message = strip_debug_quotes(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

/// Strip a single layer of `Debug` quoting from `s` if present, so a
/// message that arrived as `"text"` (with literal surrounding quotes)
/// renders as `text`. A no-op for the common case where `tracing`'s
/// `format_args!` `Debug` already produced an unquoted message.
fn strip_debug_quotes(s: String) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        s
    }
}

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for StageStyleFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use tracing::Level;

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        // Render through the same `core::log` helpers the StageLogger uses,
        // so a loggerless library warn carries the identical palette, label,
        // AND section indent as the surrounding `[stage]` lines (one output
        // authority). warn (and the rare info/debug that slip past the
        // filter) all render under the Warning style; only ERROR uses Error.
        let line = if *event.metadata().level() == Level::ERROR {
            anodizer_core::log::render_error(&visitor.message)
        } else {
            anodizer_core::log::render_warning(&visitor.message)
        };
        writeln!(writer, "{line}")
    }
}

/// Initialize tracing for `tracing::warn!`/`info!`/`debug!` calls inside the
/// pure-library code paths (e.g. `defaults_merge`). Honours `RUST_LOG` for
/// per-module filtering; falls back to `warn` so config-validation warnings
/// surface in CI even without a configured filter. Writes to stderr without
/// timestamps, in `StageLogger` visual style, so library warnings match the
/// pipeline's own logger output (one output authority).
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .event_format(StageStyleFormat)
        .try_init();
}

fn main() {
    enable_ci_colors();
    init_tracing();

    let brontes_cfg = brontes::Config::default().tool_name_prefix("anodizer");
    let augmented = Cli::command().subcommand(brontes::command(Some(&brontes_cfg)));
    let matches = augmented.clone().get_matches();

    if let Some(("mcp", sub)) = matches.subcommand() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| {
                eprintln!(
                    "{}",
                    anodizer_core::log::render_error(&format!(
                        "failed to start tokio runtime: {e}"
                    ))
                );
                std::process::exit(1);
            });
        let result = rt.block_on(brontes::handle(sub, &augmented, Some(&brontes_cfg)));
        match result {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("{}", anodizer_core::log::render_error(&e.to_string()));
                std::process::exit(1);
            }
        }
    }

    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    // No subcommand given: print help and exit 0. Required for package-manager
    // validators (winget, chocolatey) that smoke-test the installed binary
    // with no args and treat any non-zero exit code as an installation
    // failure.
    let command = match cli.command {
        Some(c) => c,
        None => {
            let _ = Cli::command().print_help();
            println!();
            return;
        }
    };

    let result = match command {
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
            targets,
            release_notes,
            workspace,
            preflight,
            no_preflight,
            strict_preflight,
            draft,
            release_header,
            release_header_tmpl,
            release_footer,
            release_footer_tmpl,
            release_notes_tmpl,
            fail_fast,
            split,
            merge,
            publish_only,
            prepare,
            announce_only,
            resume_release,
            replace_existing,
            no_post_publish_poll,
            no_gate_submitter,
            rollback,
            simulate_failure,
            rollback_only,
            from_run,
            allow_rerun,
            allow_nondeterministic,
            summary_json,
            allow_ai_failure,
        } => {
            let duration = parse_timeout_or_exit(&timeout);

            // Resolve --auto-snapshot: if set and repo is dirty, force snapshot mode
            let effective_snapshot =
                if !snapshot && auto_snapshot && anodizer_core::git::is_git_dirty() {
                    eprintln!(
                        "{}",
                        anodizer_core::log::render_note(
                            "repo is dirty, automatically enabling snapshot mode"
                        )
                    );
                    true
                } else {
                    snapshot
                };

            let resolved_single_target = resolve_single_target(single_target);

            // --targets=<csv> mirrors --stages=<csv>: comma-split, trim,
            // drop empty tokens (trailing-comma tolerance), error on the
            // all-empty case so the user sees the typo. Conflict with
            // --single-target is enforced at the clap level.
            let resolved_targets = match parse_targets_csv(targets.as_deref()) {
                Ok(v) => v,
                Err(msg) => {
                    eprintln!("{}", anodizer_core::log::render_error(&msg));
                    std::process::exit(1);
                }
            };

            if let Err(msg) = validate_skip_values(&skip, VALID_RELEASE_SKIPS) {
                eprintln!("{}", anodizer_core::log::render_error(&msg));
                std::process::exit(1);
            }

            let parallelism = parallelism.unwrap_or_else(num_cpus);
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
                    targets: resolved_targets,
                    release_notes,
                    release_notes_tmpl,
                    workspace,
                    draft,
                    release_header,
                    release_header_tmpl,
                    release_footer,
                    release_footer_tmpl,
                    fail_fast,
                    split,
                    merge,
                    publish_only,
                    strict: cli.strict,
                    prepare,
                    announce_only,
                    resume_release,
                    replace_existing,
                    preflight,
                    no_preflight,
                    strict_preflight,
                    no_post_publish_poll,
                    no_gate_submitter,
                    rollback,
                    simulate_failure,
                    rollback_only,
                    from_run,
                    allow_rerun,
                    allow_nondeterministic,
                    summary_json,
                    allow_ai_failure,
                })
            })
        }
        Commands::Build {
            crate_names,
            timeout,
            parallelism,
            single_target,
            workspace,
            output,
            skip,
        } => {
            let duration = parse_timeout_or_exit(&timeout);
            let parallelism = parallelism.unwrap_or_else(num_cpus);
            let config_override = cli.config.clone();
            let resolved_single_target = resolve_single_target(single_target);
            let verbose = cli.verbose;
            let debug = cli.debug;
            let quiet = cli.quiet;

            if let Err(msg) = validate_skip_values(&skip, VALID_BUILD_SKIPS) {
                eprintln!("{}", anodizer_core::log::render_error(&msg));
                std::process::exit(1);
            }

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
                    output,
                    skip,
                })
            })
        }
        Commands::Check { cmd } => match cmd {
            CheckCmd::Config { workspace } => commands::check::config::run(
                cli.config.as_deref(),
                workspace.as_deref(),
                cli.verbose,
                cli.debug,
                cli.quiet,
            ),
            CheckCmd::Determinism(args) => commands::check::determinism::run(args),
        },
        Commands::Init => commands::init::run(),
        Commands::Changelog {
            crate_name,
            from,
            to,
            output,
            snapshot,
        } => commands::changelog::run(commands::changelog::ChangelogOpts {
            crate_name,
            from,
            to,
            output,
            snapshot,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
        }),
        Commands::Completion { shell } => commands::completion::run(shell),
        Commands::Healthcheck => commands::healthcheck::run(),
        Commands::Man => {
            let cmd = anodizer_cli::build_cli();
            let man = clap_mangen::Man::new(cmd);
            let mut buf = Vec::new();
            man.render(&mut buf)
                .map_err(|e| anyhow::anyhow!("failed to render man page: {}", e))
                .and_then(|()| {
                    std::io::Write::write_all(&mut std::io::stdout(), &buf)
                        .map_err(|e| anyhow::anyhow!("failed to write man page: {}", e))
                })
        }
        Commands::Jsonschema => commands::jsonschema::run(),
        Commands::Targets { json, crate_names } => {
            commands::targets::run(commands::targets::TargetsOpts {
                json,
                crate_names,
                config_override: cli.config.clone(),
            })
        }
        Commands::ResolveTag { tag, json } => {
            commands::resolve_tag::run(commands::resolve_tag::ResolveTagOpts {
                tag,
                json,
                config_override: cli.config.clone(),
            })
        }
        Commands::Tag {
            dry_run,
            custom_tag,
            default_bump,
            crate_name,
            sub,
        } => match sub {
            Some(TagSub::Rollback {
                sha,
                dry_run: rb_dry_run,
                no_push,
                scope,
                mode,
                branch,
            }) => {
                use commands::tag::rollback::{Mode, RollbackOpts, Scope};
                (|| -> anyhow::Result<()> {
                    let scope: Scope = scope.parse().map_err(anyhow::Error::msg)?;
                    let mode: Mode = mode.parse().map_err(anyhow::Error::msg)?;
                    commands::tag::rollback::run(RollbackOpts {
                        sha,
                        dry_run: rb_dry_run,
                        no_push,
                        scope,
                        mode,
                        branch,
                        verbose: cli.verbose,
                        debug: cli.debug,
                        quiet: cli.quiet,
                    })
                })()
            }
            None => commands::tag::run(commands::tag::TagOpts {
                dry_run,
                custom_tag,
                default_bump,
                crate_name,
                config_override: cli.config.clone(),
                verbose: cli.verbose,
                debug: cli.debug,
                quiet: cli.quiet,
                strict: cli.strict,
            }),
        },
        Commands::Continue {
            merge,
            dist,
            dry_run,
            skip,
            token,
        } => commands::continue_cmd::run(commands::continue_cmd::ContinueOpts {
            dist,
            dry_run,
            skip,
            token,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
            merge,
        }),
        Commands::Publish {
            dry_run,
            token,
            dist,
            merge,
        } => commands::publish_cmd::run(commands::publish_cmd::PublishOpts {
            dry_run,
            token,
            dist,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
            merge,
        }),
        Commands::Bump {
            level_or_version,
            package,
            workspace,
            exclude,
            pre,
            exact,
            allow_dirty,
            yes,
            dry_run,
            commit,
            sign,
            commit_message,
            output,
        } => commands::bump::run(commands::bump::BumpOpts {
            level_or_version,
            package,
            workspace,
            exclude,
            pre,
            exact,
            allow_dirty,
            yes,
            dry_run,
            commit,
            sign,
            commit_message,
            output,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
            strict: cli.strict,
        }),
        Commands::Announce {
            dry_run,
            dist,
            token,
            skip,
            merge,
        } => commands::announce_cmd::run(commands::announce_cmd::AnnounceOpts {
            dry_run,
            dist,
            token,
            skip,
            config_override: cli.config.clone(),
            verbose: cli.verbose,
            debug: cli.debug,
            quiet: cli.quiet,
            merge,
        }),
    };
    if let Err(e) = result {
        eprintln!("{}", anodizer_core::log::render_error(&e.to_string()));
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
    use anodizer_cli::num_cpus;
    use clap::{CommandFactory, Parser};

    #[test]
    fn test_cli_parses_release_with_new_flags() {
        let cli = Cli::try_parse_from([
            "anodizer",
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
    fn test_cli_parses_release_with_prepare_flag() {
        // GoReleaser Pro `--prepare`: local prep stages, no upstream publish.
        let cli = Cli::try_parse_from(["anodizer", "release", "--prepare"]);
        assert!(
            cli.is_ok(),
            "CLI should parse --prepare: {:?}",
            cli.as_ref().err()
        );
        if let Ok(c) = cli
            && let Some(Commands::Release { prepare, .. }) = c.command
        {
            assert!(prepare, "prepare bool should be true");
        } else {
            panic!("expected Release command with prepare=true");
        }
    }

    #[test]
    fn test_cli_parses_release_parallelism_short() {
        let cli = Cli::try_parse_from(["anodizer", "release", "-p", "2"]);
        assert!(
            cli.is_ok(),
            "CLI should parse -p shorthand: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_build_with_new_flags() {
        let cli =
            Cli::try_parse_from(["anodizer", "build", "--parallelism", "4", "--single-target"]);
        assert!(
            cli.is_ok(),
            "CLI should parse build with new flags: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_completion() {
        let cli = Cli::try_parse_from(["anodizer", "completion", "bash"]);
        assert!(
            cli.is_ok(),
            "CLI should parse completion command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_healthcheck() {
        let cli = Cli::try_parse_from(["anodizer", "healthcheck"]);
        assert!(
            cli.is_ok(),
            "CLI should parse healthcheck command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_release_default_parallelism() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { parallelism, .. }) = cli.command {
            assert!(
                parallelism.is_none(),
                "default parallelism should be None (auto-detect), got {:?}",
                parallelism
            );
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_build_default_parallelism() {
        let cli = Cli::try_parse_from(["anodizer", "build"]).unwrap();
        if let Some(Commands::Build { parallelism, .. }) = cli.command {
            assert!(
                parallelism.is_none(),
                "default parallelism should be None (auto-detect), got {:?}",
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
        let result = anodizer_cli::detect_host_target();
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
            let cli = Cli::try_parse_from(["anodizer", "completion", shell]);
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
        assert!(
            help.contains("targets"),
            "help should mention targets command"
        );
    }

    #[test]
    fn test_cli_parses_targets_json() {
        let cli = Cli::try_parse_from(["anodizer", "targets", "--json"]);
        assert!(
            cli.is_ok(),
            "CLI should parse targets --json: {:?}",
            cli.err()
        );
        if let Some(Commands::Targets { json, crate_names }) = cli.unwrap().command {
            assert!(json, "--json should be true");
            assert!(crate_names.is_empty(), "crate_names should default empty");
        } else {
            panic!("expected Targets command");
        }
    }

    #[test]
    fn test_cli_parses_targets_crate_filter() {
        let cli = Cli::try_parse_from(["anodizer", "targets", "--crate", "core", "--crate", "cli"]);
        assert!(
            cli.is_ok(),
            "CLI should parse targets --crate: {:?}",
            cli.err()
        );
        if let Some(Commands::Targets { crate_names, .. }) = cli.unwrap().command {
            assert_eq!(crate_names, vec!["core".to_string(), "cli".to_string()]);
        } else {
            panic!("expected Targets command");
        }
    }

    #[test]
    fn test_cli_parses_jsonschema() {
        let cli = Cli::try_parse_from(["anodizer", "jsonschema"]);
        assert!(
            cli.is_ok(),
            "CLI should parse jsonschema command: {:?}",
            cli.err()
        );
    }

    #[test]
    fn test_cli_parses_tag_dry_run() {
        let cli = Cli::try_parse_from(["anodizer", "tag", "--dry-run"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --dry-run: {:?}",
            cli.err()
        );
        if let Some(Commands::Tag { dry_run, .. }) = cli.unwrap().command {
            assert!(dry_run);
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_custom_tag() {
        let cli = Cli::try_parse_from(["anodizer", "tag", "--custom-tag", "v5.0.0"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --custom-tag: {:?}",
            cli.err()
        );
        if let Some(Commands::Tag { custom_tag, .. }) = cli.unwrap().command {
            assert_eq!(custom_tag, Some("v5.0.0".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_default_bump() {
        let cli = Cli::try_parse_from(["anodizer", "tag", "--default-bump", "major"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag --default-bump: {:?}",
            cli.err()
        );
        if let Some(Commands::Tag { default_bump, .. }) = cli.unwrap().command {
            assert_eq!(default_bump, Some("major".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_crate_flag() {
        let cli = Cli::try_parse_from(["anodizer", "tag", "--crate", "my-lib"]);
        assert!(cli.is_ok(), "CLI should parse tag --crate: {:?}", cli.err());
        if let Some(Commands::Tag { crate_name, .. }) = cli.unwrap().command {
            assert_eq!(crate_name, Some("my-lib".to_string()));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_all_flags() {
        let cli = Cli::try_parse_from([
            "anodizer",
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
    fn test_cli_parses_tag_rollback_bare() {
        let cli = Cli::try_parse_from(["anodizer", "tag", "rollback"]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag rollback: {:?}",
            cli.err()
        );
        if let Some(Commands::Tag { sub, .. }) = cli.unwrap().command {
            assert!(matches!(sub, Some(TagSub::Rollback { .. })));
        } else {
            panic!("expected Tag command");
        }
    }

    #[test]
    fn test_cli_parses_tag_rollback_flags() {
        let cli = Cli::try_parse_from([
            "anodizer",
            "tag",
            "rollback",
            "deadbeef",
            "--dry-run",
            "--no-push",
            "--scope",
            "lockstep",
            "--mode",
            "reset",
            "--branch",
            "master",
        ]);
        assert!(
            cli.is_ok(),
            "CLI should parse tag rollback with flags: {:?}",
            cli.err()
        );
        if let Some(Commands::Tag {
            sub:
                Some(TagSub::Rollback {
                    sha,
                    dry_run,
                    no_push,
                    scope,
                    mode,
                    branch,
                }),
            ..
        }) = cli.unwrap().command
        {
            assert_eq!(sha.as_deref(), Some("deadbeef"));
            assert!(dry_run);
            assert!(no_push);
            assert_eq!(scope, "lockstep");
            assert_eq!(mode, "reset");
            assert_eq!(branch.as_deref(), Some("master"));
        } else {
            panic!("expected Tag command with Rollback sub");
        }
    }

    #[test]
    fn test_cli_parses_release_nightly_flag() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--nightly"]);
        assert!(
            cli.is_ok(),
            "CLI should parse release --nightly: {:?}",
            cli.err()
        );
        if let Some(Commands::Release { nightly, .. }) = cli.unwrap().command {
            assert!(nightly, "--nightly flag should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_nightly_defaults_false() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { nightly, .. }) = cli.command {
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
        let cli = Cli::try_parse_from(["anodizer", "release", "--workspace", "frontend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse release --workspace: {:?}",
            cli.err()
        );
        if let Some(Commands::Release { workspace, .. }) = cli.unwrap().command {
            assert_eq!(workspace, Some("frontend".to_string()));
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_release_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { workspace, .. }) = cli.command {
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
        let cli = Cli::try_parse_from(["anodizer", "build", "--workspace", "frontend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse build --workspace: {:?}",
            cli.err()
        );
        if let Some(Commands::Build { workspace, .. }) = cli.unwrap().command {
            assert_eq!(workspace, Some("frontend".to_string()));
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_cli_build_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodizer", "build"]).unwrap();
        if let Some(Commands::Build { workspace, .. }) = cli.command {
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

    // ---- Check subcommand tests ----

    #[test]
    fn check_config_accepts_workspace_flag() {
        let cli = Cli::try_parse_from(["anodizer", "check", "config", "--workspace", "backend"]);
        assert!(
            cli.is_ok(),
            "CLI should parse check config --workspace: {:?}",
            cli.err()
        );
        if let Some(Commands::Check { cmd }) = cli.unwrap().command {
            match cmd {
                CheckCmd::Config { workspace } => {
                    assert_eq!(workspace, Some("backend".to_string()));
                }
                _ => panic!("expected Check::Config command"),
            }
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn check_config_workspace_defaults_none() {
        let cli = Cli::try_parse_from(["anodizer", "check", "config"]).unwrap();
        if let Some(Commands::Check { cmd }) = cli.command {
            match cmd {
                CheckCmd::Config { workspace } => {
                    assert!(
                        workspace.is_none(),
                        "check config --workspace should default to None"
                    );
                }
                _ => panic!("expected Check::Config command"),
            }
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn check_determinism_parses_runs() {
        let cli = Cli::try_parse_from(["anodizer", "check", "determinism", "--runs", "3"]);
        assert!(
            cli.is_ok(),
            "CLI should parse check determinism --runs: {:?}",
            cli.err()
        );
        if let Some(Commands::Check { cmd }) = cli.unwrap().command {
            match cmd {
                CheckCmd::Determinism(args) => {
                    assert_eq!(args.runs, 3);
                }
                _ => panic!("expected Check::Determinism command"),
            }
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn check_determinism_default_runs_is_2() {
        let cli = Cli::try_parse_from(["anodizer", "check", "determinism"]).unwrap();
        if let Some(Commands::Check { cmd }) = cli.command {
            match cmd {
                CheckCmd::Determinism(args) => {
                    assert_eq!(args.runs, 2, "default --runs should be 2");
                }
                _ => panic!("expected Check::Determinism command"),
            }
        } else {
            panic!("expected Check command");
        }
    }

    #[test]
    fn bare_check_prints_help_or_errors() {
        // Bare `anodize check` (no subcommand) should fail to parse — clap's
        // default for a required subcommand. Either prints help or errors;
        // both are acceptable as long as no Commands::Check is produced.
        let result = Cli::try_parse_from(["anodizer", "check"]);
        assert!(
            result.is_err(),
            "bare `anodize check` should error (subcommand required), got: {:?}",
            result.ok().and_then(|c| c.command.map(|_| "parsed"))
        );
    }

    #[test]
    fn test_help_output_check_subcommand_lists_leaves() {
        let mut cmd = Cli::command();
        let check_help = cmd
            .find_subcommand_mut("check")
            .expect("check subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            check_help.contains("config"),
            "check help should list 'config' subcommand, got: {}",
            check_help
        );
        assert!(
            check_help.contains("determinism"),
            "check help should list 'determinism' subcommand, got: {}",
            check_help
        );
    }

    #[test]
    fn test_cli_parses_quiet_flag() {
        // --quiet long form
        let cli = Cli::try_parse_from(["anodizer", "--quiet", "release"]);
        assert!(cli.is_ok(), "CLI should parse --quiet: {:?}", cli.err());
        assert!(cli.unwrap().quiet, "--quiet should set quiet to true");

        // -q short form
        let cli = Cli::try_parse_from(["anodizer", "-q", "release"]);
        assert!(cli.is_ok(), "CLI should parse -q: {:?}", cli.err());
        assert!(cli.unwrap().quiet, "-q should set quiet to true");

        // quiet defaults to false
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        assert!(!cli.quiet, "quiet should default to false");
    }

    #[test]
    fn test_cli_parses_release_draft_flag() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--draft"]);
        assert!(cli.is_ok(), "CLI should parse --draft: {:?}", cli.err());
        if let Some(Commands::Release { draft, .. }) = cli.unwrap().command {
            assert!(draft, "--draft should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_draft_defaults_false() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { draft, .. }) = cli.command {
            assert!(!draft, "--draft should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_release_header_footer() {
        let cli = Cli::try_parse_from([
            "anodizer",
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
        if let Some(Commands::Release {
            release_header,
            release_footer,
            ..
        }) = cli.unwrap().command
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
        let cli = Cli::try_parse_from(["anodizer", "release", "--split"]);
        assert!(cli.is_ok(), "CLI should parse --split: {:?}", cli.err());
        if let Some(Commands::Release { split, merge, .. }) = cli.unwrap().command {
            assert!(split, "--split should be true");
            assert!(!merge, "--merge should be false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_release_merge_flag() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--merge"]);
        assert!(cli.is_ok(), "CLI should parse --merge: {:?}", cli.err());
        if let Some(Commands::Release { split, merge, .. }) = cli.unwrap().command {
            assert!(!split, "--split should be false");
            assert!(merge, "--merge should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_split_merge_default_false() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { split, merge, .. }) = cli.command {
            assert!(!split, "--split should default to false");
            assert!(!merge, "--merge should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_split_with_single_target() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--split", "--single-target"]);
        assert!(
            cli.is_ok(),
            "CLI should parse --split --single-target: {:?}",
            cli.err()
        );
        if let Some(Commands::Release {
            split,
            single_target,
            ..
        }) = cli.unwrap().command
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

    // ---- New CLI flag tests ----

    #[test]
    fn test_cli_parses_fail_fast() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--fail-fast"]);
        assert!(cli.is_ok(), "CLI should parse --fail-fast: {:?}", cli.err());
        if let Some(Commands::Release { fail_fast, .. }) = cli.unwrap().command {
            assert!(fail_fast, "--fail-fast should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_fail_fast_defaults_false() {
        let cli = Cli::try_parse_from(["anodizer", "release"]).unwrap();
        if let Some(Commands::Release { fail_fast, .. }) = cli.command {
            assert!(!fail_fast, "--fail-fast should default to false");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_release_notes_tmpl() {
        let cli = Cli::try_parse_from([
            "anodizer",
            "release",
            "--release-notes-tmpl",
            "/tmp/notes.md.tmpl",
        ]);
        assert!(
            cli.is_ok(),
            "CLI should parse --release-notes-tmpl: {:?}",
            cli.err()
        );
        if let Some(Commands::Release {
            release_notes_tmpl, ..
        }) = cli.unwrap().command
        {
            assert_eq!(
                release_notes_tmpl,
                Some(std::path::PathBuf::from("/tmp/notes.md.tmpl"))
            );
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn test_cli_parses_build_output() {
        let cli = Cli::try_parse_from(["anodizer", "build", "-o", "./myapp"]);
        assert!(cli.is_ok(), "CLI should parse build -o: {:?}", cli.err());
        if let Some(Commands::Build { output, .. }) = cli.unwrap().command {
            assert_eq!(output, Some(std::path::PathBuf::from("./myapp")));
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_cli_parses_build_output_long() {
        let cli = Cli::try_parse_from(["anodizer", "build", "--output", "/usr/local/bin/myapp"]);
        assert!(
            cli.is_ok(),
            "CLI should parse build --output: {:?}",
            cli.err()
        );
        if let Some(Commands::Build { output, .. }) = cli.unwrap().command {
            assert_eq!(
                output,
                Some(std::path::PathBuf::from("/usr/local/bin/myapp"))
            );
        } else {
            panic!("expected Build command");
        }
    }

    #[test]
    fn test_cli_parses_man_command() {
        let cli = Cli::try_parse_from(["anodizer", "man"]);
        assert!(cli.is_ok(), "CLI should parse man command: {:?}", cli.err());
        assert!(matches!(cli.unwrap().command, Some(Commands::Man)));
    }

    #[test]
    fn test_help_output_contains_new_flags() {
        let mut cmd = Cli::command();
        let release_help = cmd
            .find_subcommand_mut("release")
            .expect("release subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            release_help.contains("--fail-fast"),
            "release help should mention --fail-fast"
        );
        assert!(
            release_help.contains("--release-notes-tmpl"),
            "release help should mention --release-notes-tmpl"
        );

        let mut cmd2 = Cli::command();
        let build_help = cmd2
            .find_subcommand_mut("build")
            .expect("build subcommand should exist")
            .render_help()
            .to_string();
        assert!(
            build_help.contains("--output"),
            "build help should mention --output"
        );
    }

    #[test]
    fn test_cli_split_merge_mutually_exclusive() {
        let result = Cli::try_parse_from(["anodizer", "release", "--split", "--merge"]);
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

    #[test]
    fn test_cli_release_crate_workspace_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "anodizer",
            "release",
            "--crate",
            "foo",
            "--workspace",
            "bar",
        ]);
        assert!(
            result.is_err(),
            "--crate and --workspace should be mutually exclusive on release"
        );
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(
            err.contains("--crate")
                || err.contains("--workspace")
                || err.contains("cannot be used"),
            "error should mention the conflicting flags: {}",
            err
        );
    }

    #[test]
    fn test_cli_build_crate_workspace_mutually_exclusive() {
        let result =
            Cli::try_parse_from(["anodizer", "build", "--crate", "foo", "--workspace", "bar"]);
        assert!(
            result.is_err(),
            "--crate and --workspace should be mutually exclusive on build"
        );
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(
            err.contains("--crate")
                || err.contains("--workspace")
                || err.contains("cannot be used"),
            "error should mention the conflicting flags: {}",
            err
        );
    }

    #[test]
    fn test_cli_check_config_workspace_has_no_crate_conflict() {
        // Check config has --workspace but no --crate, so no conflict applies.
        let result = Cli::try_parse_from(["anodizer", "check", "config", "--workspace", "bar"]);
        assert!(
            result.is_ok(),
            "check config --workspace should parse successfully: {:?}",
            result.err()
        );
    }

    // ---- Release-resilience CLI flag tests ----

    #[test]
    fn release_parses_no_gate_submitter_flag() {
        let cli =
            Cli::try_parse_from(["anodizer", "release", "--no-gate-submitter"]).expect("parses");
        if let Some(Commands::Release {
            no_gate_submitter, ..
        }) = cli.command
        {
            assert!(no_gate_submitter, "--no-gate-submitter should be true");
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_parses_rollback_none() {
        let cli =
            Cli::try_parse_from(["anodizer", "release", "--rollback", "none"]).expect("parses");
        if let Some(Commands::Release { rollback, .. }) = cli.command {
            assert_eq!(rollback.as_deref(), Some("none"));
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_parses_rollback_best_effort() {
        let cli = Cli::try_parse_from(["anodizer", "release", "--rollback", "best-effort"])
            .expect("parses");
        if let Some(Commands::Release { rollback, .. }) = cli.command {
            assert_eq!(rollback.as_deref(), Some("best-effort"));
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_simulate_failure_repeatable() {
        let cli = Cli::try_parse_from([
            "anodizer",
            "release",
            "--simulate-failure",
            "foo",
            "--simulate-failure",
            "bar",
        ])
        .expect("parses");
        if let Some(Commands::Release {
            simulate_failure, ..
        }) = cli.command
        {
            assert_eq!(simulate_failure, vec!["foo".to_string(), "bar".to_string()]);
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_from_run_requires_rollback_only() {
        // --from-run without --rollback-only should be rejected by clap
        // because the `requires = "rollback_only"` attribute fires.
        let result = Cli::try_parse_from(["anodizer", "release", "--from-run", "abc123"]);
        assert!(
            result.is_err(),
            "--from-run without --rollback-only must error"
        );
    }

    #[test]
    fn release_rollback_only_requires_from_run() {
        // Symmetric requirement: --rollback-only without --from-run errors.
        let result = Cli::try_parse_from(["anodizer", "release", "--rollback-only"]);
        assert!(
            result.is_err(),
            "--rollback-only without --from-run must error"
        );
    }

    #[test]
    fn release_from_run_rejects_path_traversal() {
        // `--from-run` is joined into a filesystem path by the
        // rollback-only replay code; clap's value_parser must reject any
        // operator-typed value that could traverse out of <dist>/run-*/.
        // The error must surface at parse time so no pipeline work runs
        // with a poisoned id.
        for bad in [
            "../etc/passwd",
            "foo/bar",
            "foo\\bar",
            "..",
            ".",
            "/abs",
            "", // clap may or may not reach the parser for empty; both outcomes are errors
            "foo bar",
            "foo;rm",
        ] {
            let result =
                Cli::try_parse_from(["anodizer", "release", "--rollback-only", "--from-run", bad]);
            assert!(
                result.is_err(),
                "--from-run={:?} must be rejected at parse time",
                bad
            );
        }
    }

    #[test]
    fn release_from_run_accepts_normal_ids() {
        // The happy-path shapes a real run_id might take. These should
        // PARSE successfully (the command may still fail later because
        // the report.json doesn't exist on disk; that's a runtime
        // concern, not a parse-time one).
        for good in [
            "abc123",
            "v1.2.3",
            "run-2026-05-14",
            "_local-test",
            "DEADBEEF",
        ] {
            let result =
                Cli::try_parse_from(["anodizer", "release", "--rollback-only", "--from-run", good]);
            assert!(
                result.is_ok(),
                "--from-run={:?} should parse, got {:?}",
                good,
                result.err()
            );
        }
    }

    #[test]
    fn release_parses_allow_nondeterministic_repeatable() {
        let cli = Cli::try_parse_from([
            "anodizer",
            "release",
            "--allow-nondeterministic",
            "foo.rpm=tool-bug",
            "--allow-nondeterministic",
            "bar.deb=other-reason",
        ])
        .expect("parses");
        if let Some(Commands::Release {
            allow_nondeterministic,
            ..
        }) = cli.command
        {
            assert_eq!(
                allow_nondeterministic,
                vec![
                    "foo.rpm=tool-bug".to_string(),
                    "bar.deb=other-reason".to_string(),
                ]
            );
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_parses_summary_json() {
        let cli =
            Cli::try_parse_from(["anodizer", "release", "--summary-json", "/tmp/summary.json"])
                .expect("parses");
        if let Some(Commands::Release { summary_json, .. }) = cli.command {
            assert_eq!(
                summary_json,
                Some(std::path::PathBuf::from("/tmp/summary.json"))
            );
        } else {
            panic!("expected Release command");
        }
    }

    #[test]
    fn release_help_lists_resilience_flags() {
        let mut cmd = Cli::command();
        let release_help = cmd
            .find_subcommand_mut("release")
            .expect("release subcommand should exist")
            .render_help()
            .to_string();
        for flag in [
            "--no-gate-submitter",
            "--rollback",
            "--rollback-only",
            "--from-run",
            "--allow-nondeterministic",
            "--summary-json",
        ] {
            assert!(
                release_help.contains(flag),
                "release help should mention {} flag",
                flag
            );
        }
        // --simulate-failure is hidden; it should NOT appear in --help.
        assert!(
            !release_help.contains("--simulate-failure"),
            "release help should NOT mention --simulate-failure (it's hide=true)"
        );
    }
}
