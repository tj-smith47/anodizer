//! Thin structured logging helper for anodizer stages.
//!
//! Provides level-gated output to stderr, with consistent `[stage] message`
//! formatting. Keeps stdout clean for machine-parseable output (e.g. `anodizer tag`).
//!
//! # Verbosity levels
//!
//! - **quiet**: errors only (for CI where only failures matter)
//! - **default**: status messages (stage start/complete, key actions)
//! - **verbose**: detail (command output, env vars, file paths)
//! - **debug**: everything (HTTP request/response, template contexts, resolved config)
//!
//! # Secret redaction
//!
//! Every `StageLogger` carries an optional env-pairs list that drives the
//! redaction policy applied inside [`StageLogger::check_output`]. Callers
//! that go through [`crate::context::Context::logger`] inherit the merged
//! `{process env, config env}` pairs automatically; manual constructors
//! (`StageLogger::new`) start with no env and can be enriched via
//! [`StageLogger::with_env`]. Stderr / stdout interpolated into log lines
//! or `bail!` messages is therefore redacted without callers having to
//! remember to scrub at every site.

use std::sync::{Arc, Mutex};

use colored::Colorize;

/// Level of a log line captured by a [`LogCapture`]. Mirrors the
/// [`StageLogger`] methods that produce each level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Status,
    Verbose,
    Debug,
}

/// In-memory sink that records every log line a [`StageLogger`] emits.
///
/// Cheap clone (`Arc<Mutex<Vec<…>>>` underneath) — pass the same handle to
/// every logger derived from a [`crate::context::Context`] and read aggregated
/// counts back via the accessor methods. Intended for tests that need to
/// assert "publisher emitted ≥N status lines" — calls still write to stderr
/// so test output stays debuggable.
#[derive(Clone, Default)]
pub struct LogCapture {
    inner: Arc<Mutex<Vec<(LogLevel, String)>>>,
}

impl LogCapture {
    /// Construct a fresh empty capture sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a log line to the capture vec. Called from the
    /// [`StageLogger`] methods when a capture is attached.
    pub(crate) fn record(&self, level: LogLevel, msg: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.push((level, msg.into()));
        }
    }

    /// Number of [`LogLevel::Status`] lines recorded.
    pub fn status_count(&self) -> usize {
        self.count(LogLevel::Status)
    }

    /// Number of [`LogLevel::Warn`] lines recorded.
    pub fn warn_count(&self) -> usize {
        self.count(LogLevel::Warn)
    }

    /// Number of [`LogLevel::Error`] lines recorded.
    pub fn error_count(&self) -> usize {
        self.count(LogLevel::Error)
    }

    /// Total count across all levels (useful sanity check).
    pub fn total_count(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    fn count(&self, level: LogLevel) -> usize {
        self.inner
            .lock()
            .map(|g| g.iter().filter(|(l, _)| *l == level).count())
            .unwrap_or(0)
    }

    /// Snapshot of every recorded line in insertion order.
    pub fn all_messages(&self) -> Vec<(LogLevel, String)> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

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

/// Stage logger: wraps a stage name, verbosity level, and an optional
/// env-pairs list used for secret redaction.
///
/// All output goes to stderr. Create one per stage via [`StageLogger::new`].
/// Prefer `Context::logger("name")` over `StageLogger::new` when a
/// `Context` is in scope, because it carries the env automatically.
///
/// ```rust,ignore
/// let log = ctx.logger("build");                  // env pre-populated
/// let log = StageLogger::new("build", verbosity)  // no env yet
///     .with_env(env_pairs);                       // attach env for redact
/// log.status("compiling for x86_64-unknown-linux-gnu");
/// log.verbose(&format!("RUSTFLAGS={}", flags));
/// log.debug(&format!("full env: {:?}", env));
/// ```
#[derive(Clone)]
pub struct StageLogger {
    stage: &'static str,
    verbosity: Verbosity,
    /// Env-pairs used to redact subprocess output and bail messages. The
    /// inner vec is shared via `Arc` so cloning a logger does not copy the
    /// env every time. `None` means redaction is a no-op (matches the
    /// behaviour before this field existed).
    env: Option<Arc<Vec<(String, String)>>>,
    /// Optional in-memory capture sink. When present, every log method also
    /// appends to the capture vec (after the stderr write). `None` means
    /// the logger only writes to stderr (production default).
    capture: Option<LogCapture>,
}

impl StageLogger {
    pub fn new(stage: &'static str, verbosity: Verbosity) -> Self {
        Self {
            stage,
            verbosity,
            env: None,
            capture: None,
        }
    }

    /// Construct a logger backed by an in-memory [`LogCapture`] alongside the
    /// usual stderr writes. Returns the logger plus a clone of the capture
    /// handle so the test can read counts back after the SUT runs.
    ///
    /// Intended exclusively for tests — production code uses
    /// [`StageLogger::new`] or [`crate::context::Context::logger`].
    pub fn with_capture(stage: &'static str, verbosity: Verbosity) -> (Self, LogCapture) {
        let capture = LogCapture::new();
        let logger = Self {
            stage,
            verbosity,
            env: None,
            capture: Some(capture.clone()),
        };
        (logger, capture)
    }

    /// Attach an existing [`LogCapture`] to this logger. Useful when the
    /// capture is owned by a [`crate::context::Context`] and every derived
    /// logger should append to the same vec.
    pub fn with_capture_handle(mut self, capture: LogCapture) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Attach an env-pairs list to drive secret redaction inside
    /// [`StageLogger::check_output`] and [`StageLogger::redact`]. The list
    /// is shared via `Arc`, so passing the same vec to many loggers does
    /// not duplicate the underlying storage.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = Some(Arc::new(env));
        self
    }

    /// Redact secret values from `s` using this logger's attached env.
    ///
    /// When no env has been attached (the default for `StageLogger::new`),
    /// returns the input unchanged. Combines `redact::string` (for
    /// known-secret env values) with `redact::redact_url_credentials`
    /// (for inline `https://<user>:<pass>@host` URL credentials that may
    /// not match any exported env-var value).
    pub fn redact(&self, s: &str) -> String {
        let credential_stripped = crate::redact::redact_url_credentials(s);
        match self.env.as_deref() {
            Some(env) => crate::redact::string(&credential_stripped, env),
            None => credential_stripped,
        }
    }

    /// Error message — always shown (even in quiet mode).
    pub fn error(&self, msg: &str) {
        eprintln!("{} [{}] {}", "Error:".red().bold(), self.stage, msg);
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Error, msg);
        }
    }

    /// Warning message — shown at Normal and above.
    pub fn warn(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!("{} [{}] {}", "Warning:".yellow().bold(), self.stage, msg);
        }
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Warn, msg);
        }
    }

    /// Status message — shown at Normal and above. This is the default level
    /// for key actions (stage start, completion, skips, dry-run notes).
    pub fn status(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!("[{}] {}", self.stage, msg);
        }
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Detail message — shown only at Verbose and above.
    /// Use for: command output on success, env vars, file paths, template vars.
    pub fn verbose(&self, msg: &str) {
        if self.verbosity >= Verbosity::Verbose {
            eprintln!("[{}] {}", self.stage, msg);
        }
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Verbose, msg);
        }
    }

    /// Debug message — shown only at Debug level.
    /// Use for: HTTP request/response details, full template contexts, resolved config.
    pub fn debug(&self, msg: &str) {
        if self.verbosity >= Verbosity::Debug {
            eprintln!("[{}] {}", self.stage.dimmed(), msg.dimmed());
        }
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Debug, msg);
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
    ///
    /// Stderr and stdout are passed through [`StageLogger::redact`] before
    /// they reach the log sink, so any secret env-var values present in the
    /// subprocess output are replaced with `$KEY_NAME` (and inline
    /// `https://<user>:<pass>@host` URL credentials are scrubbed) without
    /// callers having to remember to redact at each call site. Mirrors
    /// GoReleaser's `gio.Safe(stderr)` pattern at every subprocess
    /// boundary.
    pub fn check_output(
        &self,
        output: std::process::Output,
        label: &str,
    ) -> anyhow::Result<std::process::Output> {
        let (stderr_line, stdout_line) = self.format_output_lines(&output, label);
        if !output.status.success() {
            if let Some(line) = stderr_line {
                self.error(&line);
            }
            if let Some(line) = stdout_line {
                self.error(&line);
            }
            // Embed a (truncated, redacted) stderr tail in the bubbled
            // error so operators reading the final anyhow chain see
            // something more actionable than just an exit code. The
            // separately-emitted `log.error` lines above remain the
            // primary surface; this is defense in depth for callers
            // that propagate the error past the StageLogger context.
            let stderr_raw = String::from_utf8_lossy(&output.stderr);
            let stderr_tail = if stderr_raw.is_empty() {
                String::from("<no stderr>")
            } else {
                let redacted = self.redact(&stderr_raw);
                let trimmed = redacted.trim();
                // Cap at 2 KiB to keep error chains scannable.
                const MAX: usize = 2048;
                if trimmed.len() > MAX {
                    let cut = trimmed
                        .char_indices()
                        .nth(MAX)
                        .map(|(i, _)| i)
                        .unwrap_or(MAX);
                    format!("{}…", &trimmed[..cut])
                } else {
                    trimmed.to_string()
                }
            };
            anyhow::bail!(
                "{} failed with exit code: {}; stderr: {}",
                label,
                output.status.code().unwrap_or(-1),
                stderr_tail
            );
        }
        if self.is_verbose()
            && let Some(line) = stdout_line
        {
            self.verbose(&line);
        }
        Ok(output)
    }

    /// Compose the redacted stderr / stdout log lines that
    /// [`StageLogger::check_output`] would emit for `output`. Returned as
    /// `(stderr_line, stdout_line)` where each `Option` is `Some` only when
    /// the corresponding stream had any content. Exposed via
    /// `pub(crate)` so the redaction logic can be unit-tested without
    /// having to capture stderr (`eprintln!` cannot be intercepted from
    /// the same process portably).
    pub(crate) fn format_output_lines(
        &self,
        output: &std::process::Output,
        label: &str,
    ) -> (Option<String>, Option<String>) {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr_line = if stderr_raw.is_empty() {
            None
        } else {
            let stderr = self.redact(&stderr_raw);
            let prefix = if output.status.success() {
                "output"
            } else {
                "stderr"
            };
            // Failure messages format stderr separately from stdout (under
            // the "stderr" label); success uses one "output" label for
            // stdout only.
            if output.status.success() {
                // success path: stderr is never surfaced through check_output
                None
            } else {
                Some(format!("{label} {prefix}:\n{stderr}"))
            }
        };
        let stdout_raw = String::from_utf8_lossy(&output.stdout);
        let stdout_line = if stdout_raw.is_empty() {
            None
        } else {
            let stdout = self.redact(&stdout_raw);
            let prefix = if output.status.success() {
                "output"
            } else {
                "stdout"
            };
            Some(format!("{label} {prefix}:\n{stdout}"))
        };
        (stderr_line, stdout_line)
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

    // -----------------------------------------------------------------
    // Redaction inside check_output
    // -----------------------------------------------------------------

    #[cfg(unix)]
    fn fake_output(stdout: &[u8], stderr: &[u8], code: i32) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn test_redact_uses_attached_env() {
        // A logger built via `with_env` must scrub configured secrets.
        let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
            "GITHUB_TOKEN".to_string(),
            "ghp_real_secret_token".to_string(),
        )]);
        let out = log.redact("auth header: ghp_real_secret_token");
        assert_eq!(out, "auth header: $GITHUB_TOKEN");
        assert!(!out.contains("ghp_real_secret_token"));
    }

    #[test]
    fn test_redact_without_env_only_scrubs_inline_urls() {
        // A logger constructed without `with_env` still scrubs inline URL
        // credentials, even if the bare token is not in env (the env-pair
        // list is empty).
        let log = StageLogger::new("test", Verbosity::Normal);
        let out = log.redact("fetched from https://user:tok@example.com/path");
        assert_eq!(out, "fetched from https://<redacted>@example.com/path");
    }

    #[test]
    fn test_redact_combines_env_and_url_credentials() {
        let log = StageLogger::new("test", Verbosity::Normal)
            .with_env(vec![("API_TOKEN".to_string(), "ghp_tok123".to_string())]);
        // Both the env-value token AND the inline URL credential should be
        // scrubbed in a single call.
        let out = log.redact("remote: https://ghp_tok123@github.com/x/y");
        // URL credential strip runs first, so the `ghp_tok123` between
        // `://` and `@` becomes `<redacted>`. The path / host text never
        // contains `ghp_tok123`, so the env-value pass is a no-op here.
        assert_eq!(out, "remote: https://<redacted>@github.com/x/y");
        assert!(!out.contains("ghp_tok123"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_redacts_stderr_on_failure() {
        // Stderr from a failing subprocess must be redacted before
        // the logger surfaces it, so secrets present in `output.stderr`
        // never reach the eprintln sink (or any future log appender).
        let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
            "REGISTRY_PASSWORD".to_string(),
            "supersecret_pw_123".to_string(),
        )]);
        let output = fake_output(
            b"",
            b"docker login failed: invalid password 'supersecret_pw_123'",
            1,
        );
        let (stderr_line, _) = log.format_output_lines(&output, "docker login");
        let line = stderr_line.expect("stderr should be present on failure");
        assert!(
            !line.contains("supersecret_pw_123"),
            "stderr must be redacted: {line}"
        );
        assert!(line.contains("$REGISTRY_PASSWORD"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_redacts_stdout_on_failure() {
        // Stdout on the failure path must be redacted alongside
        // stderr. Some tools dump credentials onto stdout (e.g. helm
        // login prints a warning to stdout, not stderr).
        let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
            "DOCKER_PASSWORD".to_string(),
            "tok_dckr_abc".to_string(),
        )]);
        let output = fake_output(b"echoed config: DOCKER_PASSWORD=tok_dckr_abc\n", b"", 2);
        let (_, stdout_line) = log.format_output_lines(&output, "docker");
        let line = stdout_line.expect("stdout should be present on failure");
        assert!(!line.contains("tok_dckr_abc"));
        assert!(line.contains("$DOCKER_PASSWORD"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_redacts_stdout_on_verbose_success() {
        // At verbose level, successful subprocess stdout is logged
        // too; it must also be redacted.
        let log = StageLogger::new("test", Verbosity::Verbose).with_env(vec![(
            "MY_API_KEY".to_string(),
            "key-abcdef-123".to_string(),
        )]);
        let output = fake_output(b"echo: key-abcdef-123 OK\n", b"", 0);
        let (_, stdout_line) = log.format_output_lines(&output, "echo");
        let line = stdout_line.expect("stdout should be present on success");
        assert!(!line.contains("key-abcdef-123"));
        assert!(line.contains("$MY_API_KEY"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_strips_inline_url_credentials_without_env() {
        // A logger built without env still strips URL credentials,
        // so even when the user did not export a matching env var, an
        // inline `https://<user>:<pw>@host` in stderr is scrubbed.
        let log = StageLogger::new("test", Verbosity::Normal);
        let output = fake_output(
            b"",
            b"fatal: cannot read https://user:p4ssw0rd@example.com/repo.git\n",
            128,
        );
        let (stderr_line, _) = log.format_output_lines(&output, "git fetch");
        let line = stderr_line.expect("stderr should be present on failure");
        assert!(
            !line.contains("p4ssw0rd"),
            "userinfo must be redacted: {line}"
        );
        assert!(line.contains("<redacted>@example.com"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_bail_message_excludes_raw_secret() {
        // The bail message embeds the (truncated, redacted) stderr tail
        // so an operator reading the bubbled anyhow chain sees something
        // more actionable than the bare exit code. That redaction must
        // still strip env-resolved secrets — otherwise the new tail
        // would leak whatever stderr the subprocess emitted.
        let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
            "AUTH_TOKEN".to_string(),
            "secret_zzz_yyy".to_string(),
        )]);
        let output = fake_output(b"", b"401 Unauthorized: secret_zzz_yyy\n", 1);
        let err = log
            .check_output(output, "curl")
            .expect_err("non-zero exit should bail");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("secret_zzz_yyy"),
            "bail message leaks secret: {msg}"
        );
        assert!(
            msg.contains("stderr:") && msg.contains("401 Unauthorized"),
            "bail message should embed redacted stderr tail: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_bail_includes_no_stderr_marker_when_empty() {
        // Subprocess failed with empty stderr — the bail still wants
        // SOMETHING after `stderr:` so a grep on operator logs sees a
        // deterministic marker rather than blank text.
        let log = StageLogger::new("test", Verbosity::Normal);
        let output = fake_output(b"", b"", 7);
        let err = log
            .check_output(output, "tool")
            .expect_err("non-zero exit should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stderr: <no stderr>"),
            "expected explicit <no stderr> marker: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_output_bail_truncates_long_stderr() {
        // Stderr larger than the 2 KiB cap is truncated with an ellipsis
        // so the operator's error chain remains scannable.
        let log = StageLogger::new("test", Verbosity::Normal);
        // 3 KiB of stderr.
        let big = vec![b'x'; 3072];
        let output = fake_output(b"", &big, 1);
        let err = log
            .check_output(output, "tool")
            .expect_err("non-zero exit should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.ends_with('…'),
            "expected ellipsis on truncated stderr: {msg}"
        );
        // Truncation must keep the surface manageable — well under
        // 3 KiB of raw stderr should make it into the bail.
        assert!(
            msg.len() < 2500,
            "bail message too long: {} bytes",
            msg.len()
        );
    }

    #[test]
    fn test_with_env_is_arc_shared() {
        // Cloning a logger should share the env vec via Arc, not deep-copy.
        // Verified by pointer equality on the inner Vec backing the Arc.
        let env = vec![("K".to_string(), "v_long_enough_to_be_a_token".to_string())];
        let a = StageLogger::new("a", Verbosity::Normal).with_env(env);
        let b = a.clone();
        let pa: *const Vec<(String, String)> = a.env.as_ref().unwrap().as_ref();
        let pb: *const Vec<(String, String)> = b.env.as_ref().unwrap().as_ref();
        assert_eq!(pa, pb);
    }
}
