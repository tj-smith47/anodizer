//! Thin structured logging helper for anodize stages.
//!
//! Provides level-gated output to stderr, with consistent `[stage] message`
//! formatting. Keeps stdout clean for machine-parseable output (e.g. `anodize tag`).
//!
//! # Verbosity levels
//!
//! - **quiet**: errors only (for CI where only failures matter)
//! - **default**: status messages (stage start/complete, key actions)
//! - **verbose**: detail (command output, env vars, file paths)
//! - **debug**: everything (HTTP request/response, template contexts, resolved config)

use colored::Colorize;

/// Verbosity level, derived from CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
    Debug,
}

impl Verbosity {
    /// Derive verbosity from CLI flag combination.
    /// `--quiet` overrides `--verbose`; `--debug` overrides everything.
    pub fn from_flags(quiet: bool, verbose: bool, debug: bool) -> Self {
        if debug {
            Verbosity::Debug
        } else if quiet {
            Verbosity::Quiet
        } else if verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}

/// Stage logger — wraps a stage name and verbosity level.
///
/// All output goes to stderr. Create one per stage via [`StageLogger::new`].
///
/// ```rust,ignore
/// let log = StageLogger::new("build", ctx.verbosity());
/// log.status("compiling for x86_64-unknown-linux-gnu");
/// log.verbose(&format!("RUSTFLAGS={}", flags));
/// log.debug(&format!("full env: {:?}", env));
/// ```
pub struct StageLogger {
    stage: &'static str,
    verbosity: Verbosity,
}

impl StageLogger {
    pub fn new(stage: &'static str, verbosity: Verbosity) -> Self {
        Self { stage, verbosity }
    }

    /// Error message — always shown (even in quiet mode).
    pub fn error(&self, msg: &str) {
        eprintln!("{} [{}] {}", "Error:".red().bold(), self.stage, msg);
    }

    /// Warning message — shown at Normal and above.
    pub fn warn(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!("{} [{}] {}", "Warning:".yellow().bold(), self.stage, msg);
        }
    }

    /// Status message — shown at Normal and above. This is the default level
    /// for key actions (stage start, completion, skips, dry-run notes).
    pub fn status(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!("[{}] {}", self.stage, msg);
        }
    }

    /// Detail message — shown only at Verbose and above.
    /// Use for: command output on success, env vars, file paths, template vars.
    pub fn verbose(&self, msg: &str) {
        if self.verbosity >= Verbosity::Verbose {
            eprintln!("[{}] {}", self.stage, msg);
        }
    }

    /// Debug message — shown only at Debug level.
    /// Use for: HTTP request/response details, full template contexts, resolved config.
    pub fn debug(&self, msg: &str) {
        if self.verbosity >= Verbosity::Debug {
            eprintln!("[{}] {}", self.stage.dimmed(), msg.dimmed());
        }
    }

    /// Return the current verbosity level.
    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    /// Check if verbose output is enabled.
    pub fn is_verbose(&self) -> bool {
        self.verbosity >= Verbosity::Verbose
    }

    /// Check if debug output is enabled.
    pub fn is_debug(&self) -> bool {
        self.verbosity >= Verbosity::Debug
    }

    /// Check command output, log stderr/stdout on failure, and bail with context.
    /// On success, log stdout at verbose level. Returns `Ok(output)` on success.
    pub fn check_output(
        &self,
        output: std::process::Output,
        label: &str,
    ) -> anyhow::Result<std::process::Output> {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                self.error(&format!("{label} stderr:\n{stderr}"));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                self.error(&format!("{label} stdout:\n{stdout}"));
            }
            anyhow::bail!(
                "{} failed with exit code: {}",
                label,
                output.status.code().unwrap_or(-1)
            );
        }
        if self.is_verbose() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                self.verbose(&format!("{label} output:\n{stdout}"));
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verbosity_from_flags_default() {
        assert_eq!(
            Verbosity::from_flags(false, false, false),
            Verbosity::Normal
        );
    }

    #[test]
    fn test_verbosity_from_flags_quiet() {
        assert_eq!(Verbosity::from_flags(true, false, false), Verbosity::Quiet);
    }

    #[test]
    fn test_verbosity_from_flags_verbose() {
        assert_eq!(
            Verbosity::from_flags(false, true, false),
            Verbosity::Verbose
        );
    }

    #[test]
    fn test_verbosity_from_flags_debug() {
        assert_eq!(Verbosity::from_flags(false, false, true), Verbosity::Debug);
    }

    #[test]
    fn test_verbosity_from_flags_debug_wins_over_verbose() {
        assert_eq!(Verbosity::from_flags(false, true, true), Verbosity::Debug);
    }

    #[test]
    fn test_verbosity_from_flags_debug_wins_over_quiet() {
        assert_eq!(Verbosity::from_flags(true, false, true), Verbosity::Debug);
    }

    #[test]
    fn test_verbosity_from_flags_quiet_overrides_verbose() {
        assert_eq!(Verbosity::from_flags(true, true, false), Verbosity::Quiet);
    }

    #[test]
    fn test_verbosity_ordering() {
        assert!(Verbosity::Quiet < Verbosity::Normal);
        assert!(Verbosity::Normal < Verbosity::Verbose);
        assert!(Verbosity::Verbose < Verbosity::Debug);
    }

    #[test]
    fn test_stage_logger_is_verbose() {
        let log = StageLogger::new("test", Verbosity::Verbose);
        assert!(log.is_verbose());
        assert!(!log.is_debug());
    }

    #[test]
    fn test_stage_logger_is_debug() {
        let log = StageLogger::new("test", Verbosity::Debug);
        assert!(log.is_verbose());
        assert!(log.is_debug());
    }

    #[test]
    fn test_stage_logger_normal_not_verbose() {
        let log = StageLogger::new("test", Verbosity::Normal);
        assert!(!log.is_verbose());
        assert!(!log.is_debug());
    }

    #[test]
    fn test_default_verbosity_is_normal() {
        assert_eq!(Verbosity::default(), Verbosity::Normal);
    }
}
