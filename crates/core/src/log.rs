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

use std::sync::Arc;
#[cfg(feature = "test-helpers")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use colored::Colorize;

/// Process-global section nesting depth. Drives the 2-space-per-level
/// indentation applied to every stderr log line so output produced
/// inside a [`StageLogger::group`] sits visually beneath its header.
///
/// A single atomic (rather than per-logger state) is correct because the
/// release pipeline drives one stderr stream and no `group()` is ever
/// opened from a worker thread — sections bracket whole stages on the
/// main thread, while a stage's interior parallelism (e.g. `build`
/// spawning per-target threads) emits *inside* an already-open section.
/// The depth is therefore a property of "where the main thread is in the
/// run", not of any individual logger clone or worker.
static SECTION_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Width of the right-aligned verb column in [`StageLogger::step`],
/// matching Cargo's `   Compiling foo` look (3 leading spaces + 9-char
/// verb = a 12-column gutter before the message).
const VERB_COLUMN: usize = 12;

/// Map a pipeline stage name to its Cargo-style header verb (present
/// participle, right-aligned into the [`VERB_COLUMN`] gutter). Drives
/// [`StageLogger::group`]'s local header so a section opens with
/// `   Building build` / ` Publishing publish` instead of a bare bullet,
/// matching `cargo`'s `   Compiling foo` look.
///
/// Falls back to a capitalized "Running" for any stage without a bespoke
/// verb, so a newly-added stage still renders in the system vocabulary
/// without a code change here.
pub fn stage_verb(stage: &str) -> &'static str {
    match stage {
        "setup" => "Preparing",
        "build" => "Building",
        "upx" => "Compressing",
        "appbundle" => "Bundling",
        "dmg" | "msi" | "nsis" | "pkg" | "srpm" | "nfpm" | "makeself" => "Packaging",
        "notarize" => "Notarizing",
        "changelog" => "Changelog",
        "archive" => "Archiving",
        "source" => "Archiving",
        "snapcraft" | "flatpak" => "Packaging",
        "sbom" => "Cataloging",
        "templatefiles" => "Templating",
        "checksum" => "Checksumming",
        "sign" | "docker-sign" => "Signing",
        "before-publish" => "Preparing",
        "release" => "Releasing",
        "docker" => "Building",
        "publish" | "blob" | "snapcraft-publish" => "Publishing",
        "announce" => "Announcing",
        "publisher-summary" => "Summary",
        "finalize" => "Finalizing",
        "prepare" => "Preparing",
        _ => "Running",
    }
}

/// Render the themed `Warning:` line for `msg`, including the current
/// section indent. The single source of truth for the warning palette
/// and label, shared by [`StageLogger::warn`] and the CLI's tracing
/// formatter so a library-side `warn!` looks identical to a logger warn
/// (one output authority). The `[stage]` tag is the caller's
/// responsibility — loggerless library warns have no stage to name.
pub fn render_warning(msg: &str) -> String {
    format!("{}{} {}", indent(), "Warning:".yellow().bold(), msg)
}

/// Render the themed `Error:` line for `msg`, including the current
/// section indent. Companion to [`render_warning`]; shared so the error
/// palette/label lives in exactly one place.
pub fn render_error(msg: &str) -> String {
    format!("{}{} {}", indent(), "Error:".red().bold(), msg)
}

/// Render the themed `Note:` line for `msg`, including the current
/// section indent. The third (and final) status label in the vocabulary
/// — informational lines that are neither warnings nor errors (host-target
/// selection, auto-snapshot activation). Bold-green to read as a benign
/// status, distinct from the yellow `Warning:` and red `Error:`. Shared
/// so the `Note:` palette/label lives in exactly one place rather than
/// being open-coded per call site.
pub fn render_note(msg: &str) -> String {
    format!("{}{} {}", indent(), "Note:".green().bold(), msg)
}

/// `true` when running under GitHub Actions, where `::group::` /
/// `::endgroup::` workflow commands render collapsible log sections.
fn in_github_actions() -> bool {
    std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
}

/// Current indentation prefix (2 spaces per open section). Empty at the
/// top level. Suppressed under GitHub Actions, where `::group::` already
/// supplies the visual nesting and a leading-space prefix would only add
/// noise to the collapsed view.
///
/// Exposed so the CLI's loggerless `tracing` warning formatter can apply
/// the same indent a library warn fired mid-stage would otherwise lack,
/// keeping it aligned with the surrounding `[stage]` lines.
pub fn indent() -> String {
    if in_github_actions() {
        return String::new();
    }
    "  ".repeat(SECTION_DEPTH.load(Ordering::Relaxed))
}

/// RAII guard returned by [`StageLogger::group`]. Closes the section
/// (emits `::endgroup::` under Actions, decrements the local indent
/// depth) when dropped, so a stage's output is always balanced even if
/// the stage bails early with `?`.
#[must_use = "dropping the guard immediately ends the section"]
pub struct SectionGuard {
    _private: (),
}

impl Drop for SectionGuard {
    fn drop(&mut self) {
        if in_github_actions() {
            eprintln!("::endgroup::");
        } else {
            SECTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Level of a log line captured by a [`LogCapture`]. Mirrors the
/// [`StageLogger`] methods that produce each level.
///
/// Gated behind the `test-helpers` Cargo feature — production binaries
/// do not link the capture infrastructure.
#[cfg(feature = "test-helpers")]
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
///
/// Gated behind the `test-helpers` Cargo feature.
#[cfg(feature = "test-helpers")]
#[derive(Clone, Default)]
pub struct LogCapture {
    inner: Arc<Mutex<Vec<(LogLevel, String)>>>,
}

#[cfg(feature = "test-helpers")]
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

    /// Snapshot of every [`LogLevel::Warn`] message in insertion order.
    ///
    /// Convenience accessor for tests that care only about warns — strips
    /// the level tuple [`all_messages`] returns so callers can write
    /// `cap.warn_messages().iter().any(|m| m.contains("..."))` without
    /// the per-call filter+map boilerplate.
    ///
    /// [`all_messages`]: Self::all_messages
    pub fn warn_messages(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|(lvl, _)| *lvl == LogLevel::Warn)
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default()
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
    ///
    /// Gated behind the `test-helpers` Cargo feature — production binaries
    /// do not carry the field, so no per-log-call `is_none()` check fires.
    #[cfg(feature = "test-helpers")]
    capture: Option<LogCapture>,
}

impl StageLogger {
    pub fn new(stage: &'static str, verbosity: Verbosity) -> Self {
        Self {
            stage,
            verbosity,
            env: None,
            #[cfg(feature = "test-helpers")]
            capture: None,
        }
    }

    /// Construct a logger backed by an in-memory [`LogCapture`] alongside the
    /// usual stderr writes. Returns the logger plus a clone of the capture
    /// handle so the test can read counts back after the SUT runs.
    ///
    /// Intended exclusively for tests — production code uses
    /// [`StageLogger::new`] or [`crate::context::Context::logger`].
    ///
    /// Gated behind the `test-helpers` Cargo feature.
    #[cfg(feature = "test-helpers")]
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
    ///
    /// Gated behind the `test-helpers` Cargo feature.
    #[cfg(feature = "test-helpers")]
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

    /// Derive a clone of this logger tagged for a different `stage`, keeping
    /// verbosity, the (Arc-shared) redaction env, and any capture sink.
    ///
    /// The pipeline driver owns one `[release]`-tagged logger but brackets
    /// sub-sections (`setup`, `finalize`, `publisher-summary`) with their own
    /// `group()`. Body lines emitted inside such a section must carry the
    /// *section's* tag, not `[release]`, or the output reads
    /// `[release] wrote …` underneath `::group::finalize`. Retagging once at
    /// the section boundary lets every helper called within the section emit
    /// under the correct tag without threading an explicit `stage` argument
    /// through each call.
    pub fn with_stage(&self, stage: &'static str) -> Self {
        Self {
            stage,
            verbosity: self.verbosity,
            env: self.env.clone(),
            #[cfg(feature = "test-helpers")]
            capture: self.capture.clone(),
        }
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
        let tagged = format!("{} {}", format!("[{}]", self.stage).dimmed(), msg);
        eprintln!("{}", render_error(&tagged));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Error, msg);
        }
    }

    /// Error message rendered under an explicit `stage` tag rather than this
    /// logger's own [`Self::stage`]. Companion to [`Self::status_as`] for the
    /// pipeline driver, which owns a single `[release]`-tagged logger but
    /// opens a `group()` per stage: a stage-failure line emitted from that
    /// driver must carry the failing stage's tag (so the error inside
    /// `::group::build` reads `[build]`, not `[release]`). Identical
    /// formatting to [`Self::error`] otherwise.
    pub fn error_as(&self, stage: &str, msg: &str) {
        let tagged = format!("{} {}", format!("[{stage}]").dimmed(), msg);
        eprintln!("{}", render_error(&tagged));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Error, msg);
        }
    }

    /// Warning message — shown at Normal and above.
    pub fn warn(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            let tagged = format!("{} {}", format!("[{}]", self.stage).dimmed(), msg);
            eprintln!("{}", render_warning(&tagged));
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Warn, msg);
        }
    }

    /// Status message — shown at Normal and above. This is the default level
    /// for key actions (stage start, completion, skips, dry-run notes).
    ///
    /// Delegates to [`Self::status_as`] under this logger's own stage tag so
    /// there is a single status render path (one source of truth for the
    /// blank-line / indent / `[stage]` formatting).
    pub fn status(&self, msg: &str) {
        self.status_as(self.stage, msg);
    }

    /// Status message rendered under an explicit `stage` tag rather than
    /// this logger's own [`Self::stage`]. The pipeline driver owns a single
    /// `[release]`-tagged logger but opens a `group()` per stage; a skip /
    /// no-binary note emitted from that driver must carry the *current*
    /// stage's tag (so a note inside `::group::publish` reads `[publish]`,
    /// not `[release]`). Identical formatting to [`Self::status`] otherwise.
    pub fn status_as(&self, stage: &str, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            // Preserve fully-blank spacer lines exactly (no prefix and no
            // indent), so callers using `status("")` for vertical rhythm
            // keep a clean blank line even inside a group. An indented
            // "blank" line (trailing spaces only) would render as visible
            // whitespace and break the rhythm the caller asked for.
            if msg.is_empty() {
                eprintln!();
            } else {
                eprintln!("{}{} {}", indent(), format!("[{stage}]").dimmed(), msg);
            }
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Cargo-style status line: a capitalized, right-aligned, bold-green
    /// `verb` in a fixed-width gutter followed by `msg`
    /// (`   Building build`, `   Signing sign`). Shown at Normal and
    /// above. Use for section/stage headers where there is a natural
    /// verb; plain key-action lines stay on [`StageLogger::status`].
    pub fn step(&self, verb: &str, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!(
                "{}{} {}",
                indent(),
                format!("{verb:>VERB_COLUMN$}").green().bold(),
                msg
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Open a log section titled `title`.
    ///
    /// Under GitHub Actions emits a `::group::<title>` workflow command
    /// (rendered as a collapsible block); the returned [`SectionGuard`]
    /// emits the matching `::endgroup::` on drop. Locally emits a
    /// Cargo-style header — a bold-green right-aligned verb (derived from
    /// the stage name via [`stage_verb`]) in the [`VERB_COLUMN`] gutter
    /// followed by the title (`   Building build`) — and indents every
    /// subsequent log line two spaces until the guard drops. Sections
    /// nest.
    ///
    /// ```rust,ignore
    /// let _section = log.group("build");                 //    Building build
    /// log.status("compiling x86_64-unknown-linux-gnu");  // indented beneath
    /// // section closes here as `_section` drops
    /// ```
    #[must_use = "the section stays open only while the guard is alive"]
    pub fn group(&self, title: &str) -> SectionGuard {
        if self.verbosity >= Verbosity::Normal {
            if in_github_actions() {
                eprintln!("::group::{title}");
            } else {
                self.step(stage_verb(title), title);
                SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
            }
        } else if in_github_actions() {
            // Keep group markers balanced even in quiet mode so the
            // Actions UI never shows an unterminated section.
            eprintln!("::group::{title}");
        } else {
            SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
        }
        SectionGuard { _private: () }
    }

    /// Open a log section *without* emitting the Cargo-style verb header.
    ///
    /// Companion to [`Self::group`] for stages the pipeline skips: a skipped
    /// stage has nothing to announce in the present-participle voice
    /// (`Announcing announce` followed by `[announce] skipped` reads as a
    /// contradiction), so the driver opens the section silently and emits a
    /// single neutral `Skipped …` line itself. The `::group::`/`::endgroup::`
    /// pair (and the local indent depth) is still balanced so the Actions UI
    /// shows one collapsible block per stage exactly as [`Self::group`] does.
    #[must_use = "the section stays open only while the guard is alive"]
    pub fn group_silent(&self, title: &str) -> SectionGuard {
        if in_github_actions() {
            // Always emit the marker (even in quiet mode) so the Actions UI
            // never shows an unterminated section.
            eprintln!("::group::{title}");
        } else {
            SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
        }
        SectionGuard { _private: () }
    }

    /// Detail message — shown only at Verbose and above.
    /// Use for: command output on success, env vars, file paths, template vars.
    pub fn verbose(&self, msg: &str) {
        if self.verbosity >= Verbosity::Verbose {
            eprintln!(
                "{}{} {}",
                indent(),
                format!("[{}]", self.stage).dimmed(),
                msg
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Verbose, msg);
        }
    }

    /// Debug message — shown only at Debug level.
    /// Use for: HTTP request/response details, full template contexts, resolved config.
    pub fn debug(&self, msg: &str) {
        if self.verbosity >= Verbosity::Debug {
            eprintln!(
                "{}{} {}",
                indent(),
                format!("[{}]", self.stage).dimmed(),
                msg.dimmed()
            );
        }
        #[cfg(feature = "test-helpers")]
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

    /// Serializes the section-depth tests: `SECTION_DEPTH` is a
    /// process-global atomic, so two grouping tests running on parallel
    /// threads would observe each other's increments.
    static SECTION_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_group_guard_balances_depth_locally() {
        // GITHUB_ACTIONS must be unset so the local (indent-depth) path
        // runs rather than the `::group::` emit path.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        // SAFETY: single-threaded under SECTION_TEST_LOCK; no other
        // thread reads GITHUB_ACTIONS while this test holds the lock.
        unsafe {
            std::env::remove_var("GITHUB_ACTIONS");
        }
        let log = StageLogger::new("build", Verbosity::Normal);
        let start = SECTION_DEPTH.load(Ordering::Relaxed);
        {
            let _outer = log.group("build");
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
            {
                let _inner = log.group("sign");
                assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 2);
            }
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
        }
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
    }

    #[test]
    fn test_group_quiet_still_tracks_local_depth() {
        // Even at Quiet verbosity the local indent depth must stay
        // balanced so any status lines that DO print (errors) indent
        // correctly and the guard's decrement has a matching increment.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        // SAFETY: single-threaded under SECTION_TEST_LOCK.
        unsafe {
            std::env::remove_var("GITHUB_ACTIONS");
        }
        let log = StageLogger::new("build", Verbosity::Quiet);
        let start = SECTION_DEPTH.load(Ordering::Relaxed);
        {
            let _s = log.group("build");
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
        }
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
    }

    #[test]
    fn test_indent_suppressed_under_github_actions() {
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        // SAFETY: single-threaded under SECTION_TEST_LOCK.
        unsafe {
            std::env::set_var("GITHUB_ACTIONS", "true");
        }
        // Under Actions, indent() returns empty regardless of any
        // residual depth, because `::group::` supplies the nesting.
        assert_eq!(indent(), "");
        assert!(in_github_actions());
        // SAFETY: single-threaded under SECTION_TEST_LOCK.
        unsafe {
            std::env::remove_var("GITHUB_ACTIONS");
        }
        assert!(!in_github_actions());
    }

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
