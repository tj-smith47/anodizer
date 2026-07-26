//! `StageLogger` — the per-stage emitter, its redaction policy, and the
//! subprocess output check that surfaces a failed child's streams.

use std::sync::atomic::Ordering;

use super::RedactionEnv;
#[cfg(feature = "test-helpers")]
use super::capture::{LogCapture, LogLevel};
use super::depth::{SECTION_DEPTH, SectionGuard, current_depth};
use super::render::{
    BODY_INDENT, MARKER_DETAIL, MARKER_FAILURE, MARKER_SUCCESS, PENDING, PendingHeader,
    flush_pending, indent, render_error, render_header, render_warning, stage_header, strip_ansi,
};
use super::verbosity::Verbosity;
use std::sync::Arc;
use std::sync::Mutex;

use colored::Colorize;

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
/// log.debug(&format!("full env = {:?}", env));
/// ```
#[derive(Clone)]
pub struct StageLogger {
    /// The logger's stage identity. No longer printed (the per-line
    /// `[stage]` tag was dropped for the unified body style — section
    /// headers name the stage instead), but retained as the constructor
    /// contract: callers build a logger per stage via [`Self::new`] /
    /// [`crate::context::Context::logger`] and retag sub-sections via
    /// [`Self::with_stage`]. Kept so those entry points keep a stable
    /// signature.
    #[allow(dead_code)]
    pub(super) stage: &'static str,
    verbosity: Verbosity,
    /// Env-pairs used to redact subprocess output and bail messages, behind
    /// a shared `Arc<Mutex<_>>` cell rather than a frozen `Arc<Vec<_>>`.
    /// [`crate::context::Context::logger`] hands every `StageLogger` a clone
    /// of the SAME cell it refreshes on every `env_source` mutation (e.g.
    /// the crates.io Trusted-Publishing token mint), so a logger constructed
    /// before a mid-run credential mint still redacts secrets minted
    /// afterward — [`StageLogger::redact`] reads the cell at call time, not
    /// a snapshot taken at construction. `StageLogger::with_env` wraps its
    /// argument in a private, never-mutated cell, so manual construction
    /// keeps its previous frozen-table behavior. `None` means redaction is a
    /// no-op (matches the behaviour before this field existed).
    pub(super) env: Option<RedactionEnv>,
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
    /// [`StageLogger::check_output`] and [`StageLogger::redact`]. Wraps the
    /// list in a private cell that nothing else mutates — a frozen table,
    /// same as before this method's underlying storage moved to
    /// `Arc<Mutex<_>>`. Use `StageLogger::with_shared_env` to attach a
    /// cell that can still change after construction.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = Some(Arc::new(Mutex::new(env)));
        self
    }

    /// Attach a shared, mutable redaction cell: unlike [`StageLogger::with_env`],
    /// updates the caller makes to `env` AFTER this call are visible to
    /// every clone of this logger, because [`StageLogger::redact`] reads
    /// the cell live rather than a snapshot taken here.
    ///
    /// Used exclusively by [`crate::context::Context::logger`], which hands
    /// out clones of its own live redaction cell — refreshed on every
    /// `env_source` mutation — so a logger built before a mid-run
    /// credential mint (e.g. crates.io Trusted Publishing's
    /// `CARGO_REGISTRY_TOKEN`) still redacts it.
    pub(crate) fn with_shared_env(mut self, env: RedactionEnv) -> Self {
        self.env = Some(env);
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

    /// Redact secret values from `s` using this logger's attached env,
    /// re-read from the cell at call time (see `Self::with_shared_env`) —
    /// a secret added to a live cell after this logger was constructed is
    /// still masked.
    ///
    /// When no env has been attached (the default for `StageLogger::new`),
    /// returns the input unchanged. Combines `redact::string` (for
    /// known-secret env values) with `redact::redact_url_credentials`
    /// (for inline `https://<user>:<pass>@host` URL credentials that may
    /// not match any exported env-var value).
    pub fn redact(&self, s: &str) -> String {
        match &self.env {
            Some(env) => {
                let table = env.lock().unwrap_or_else(|e| e.into_inner());
                crate::redact::with_env(s, &table)
            }
            None => crate::redact::redact_url_credentials(s),
        }
    }

    /// Render a body line: the current section indent, the 3-space body
    /// indent, a colored `marker`, one space, then `text`. The single source
    /// of truth for the `•` / `✓` / `✗` body register so every marker line
    /// aligns byte-identically under its section header.
    pub(super) fn render_body(marker: &str, text: &str) -> String {
        format!("{}{}{} {}", indent(), BODY_INDENT, marker, text)
    }

    /// Error message — always shown (even in quiet mode). Renders the
    /// `Error` status label gutter-aligned beneath the current section.
    pub fn error(&self, msg: &str) {
        flush_pending();
        eprintln!("{}", render_error(msg));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Error, msg);
        }
    }

    /// Warning message — shown at Normal and above. Renders the `Warning`
    /// status label gutter-aligned beneath the current section.
    pub fn warn(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            flush_pending();
            eprintln!("{}", render_warning(msg));
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Warn, msg);
        }
    }

    /// Status message — shown at Normal and above. This is the default level
    /// for key actions (stage start, completion, skips, dry-run notes).
    ///
    /// Renders as a `•` detail body line beneath the current section. An
    /// empty `msg` is preserved as a bare blank spacer line (no marker, no
    /// indent) so callers using `status("")` for vertical rhythm keep a
    /// clean blank even inside a group. For an explicit register, prefer
    /// [`Self::detail`] / [`Self::success`] / [`Self::failure`].
    pub fn status(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            if msg.is_empty() {
                // A marker on a "blank" line would render as a stray bullet;
                // emit a truly empty line to preserve the caller's rhythm.
                // A blank spacer is NOT a real body line, so it does not flush
                // pending headers (a no-op section must stay invisible).
                eprintln!();
            } else {
                flush_pending();
                eprintln!(
                    "{}",
                    Self::render_body(&MARKER_DETAIL.cyan().to_string(), msg)
                );
            }
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Info / detail body line — a cyan `•` marker, then `msg`, at the body
    /// indent beneath the current section. Shown at Normal and above. The
    /// explicit-register sibling of [`Self::status`] for callers that want to
    /// name the `•` style directly.
    pub fn detail(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(&MARKER_DETAIL.cyan().to_string(), msg)
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Success body line — a green `✓` marker, then `msg`, at the body indent
    /// beneath the current section. Shown at Normal and above. Use for a
    /// completed unit of work (`✓ x86_64-… 1.2 MiB`, `✓ signed 6 artifacts`).
    pub fn success(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(&MARKER_SUCCESS.green().to_string(), msg)
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Failure body line — a red `✗` marker, then `msg`, at the body indent
    /// beneath the current section. Shown at Normal and above. Use for a
    /// failed unit of work that is reported inline (the run continues or the
    /// error is surfaced separately via [`Self::error`]).
    pub fn failure(&self, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(&MARKER_FAILURE.red().to_string(), msg)
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Whether liveness heartbeats should surface: only at exactly Normal
    /// verbosity. Quiet shows errors only, and at Verbose/Debug the live
    /// subprocess tee already conveys progress, so a heartbeat there would
    /// double the signal. The single authority for the suppression policy:
    /// the drivers consult it via `progress::heartbeat_period` before spawning
    /// any ticker, and [`Self::heartbeat`] re-checks it as defense-in-depth for
    /// a direct caller — both gates read this one predicate, never a second
    /// copy of the rule.
    pub fn heartbeats_enabled(&self) -> bool {
        self.verbosity == Verbosity::Normal
    }

    /// Heartbeat / liveness body line — a cyan `•` marker, then `msg`. Renders a
    /// slow operation (`still running cargo publish (2m15s)`) so it is not
    /// mistaken for a hang. Shown only when [`Self::heartbeats_enabled`], and
    /// recorded at its own `LogLevel::Heartbeat` level.
    pub fn heartbeat(&self, msg: &str) {
        if !self.heartbeats_enabled() {
            return;
        }
        flush_pending();
        eprintln!(
            "{}",
            Self::render_body(&MARKER_DETAIL.cyan().to_string(), msg)
        );
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Heartbeat, msg);
        }
    }

    /// Key/value meta row — a `•` detail line whose lowercase dimmed `key` is
    /// left-padded to `key_width` so the values line up within a group, then
    /// the `value`. Shown at Normal and above.
    ///
    /// Lowercase keys must never sit in the verb gutter (that column is for
    /// bold capitalized verbs only), so meta rows render in the body
    /// register. Callers that emit several rows pass the width of their
    /// widest key as `key_width` so the values share a column:
    ///
    /// ```rust,ignore
    /// let w = ["targets", "stages", "runs"].iter().map(|k| k.len()).max().unwrap();
    /// log.kv("targets", "aarch64-pc-windows-msvc", w);
    /// log.kv("stages", "build, source, sign", w);
    /// log.kv("runs", "2", w);
    /// //   • targets  aarch64-pc-windows-msvc
    /// //   • stages   build, source, sign
    /// //   • runs     2
    /// ```
    pub fn kv(&self, key: &str, value: &str, key_width: usize) {
        if self.verbosity >= Verbosity::Normal {
            // Pad the PLAIN key to width before coloring — padding the
            // already-dimmed string would count the ANSI escape bytes toward
            // the field width and misalign the value column. Two spaces after
            // the padded key give a readable gutter without a separator glyph.
            let padded = format!("{key:<key_width$}");
            let row = format!("{}  {}", padded.dimmed(), value);
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(&MARKER_DETAIL.cyan().to_string(), &row)
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, format!("{key} = {value}"));
        }
    }

    /// Cargo-style status line: a capitalized, right-aligned, bold-green
    /// `verb` in a fixed-width gutter followed by `msg`
    /// (`   Building binaries`, `   Signing artifacts`). Shown at Normal and
    /// above. Use for section/stage headers where there is a natural
    /// verb; plain key-action lines stay on [`StageLogger::status`].
    pub fn step(&self, verb: &str, msg: &str) {
        if self.verbosity >= Verbosity::Normal {
            eprintln!("{}", render_header(current_depth(), verb, msg));
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Status, msg);
        }
    }

    /// Open a log section for stage `title`.
    ///
    /// The Cargo-style header (derived from [`stage_header`]: the phrase's
    /// leading verb bold-green and right-aligned in the `VERB_COLUMN`
    /// gutter, then one space and the remaining words — `   Building binaries`,
    /// ` Publishing` for a single-word phrase) is *deferred*: it prints only
    /// when this section emits its first real body line, matching GoReleaser
    /// (a section header appears only once the section has output). A stage
    /// that does nothing therefore prints no header at all — no bare
    /// `Verifying release` over an empty body. The header renders identically
    /// everywhere — locally and under GitHub Actions — because anodizer streams
    /// one continuous log; the body indentation (not a collapsible `::group::`
    /// block) conveys nesting. Every subsequent log line is indented two spaces
    /// until the guard drops. Sections nest.
    ///
    /// ```rust,ignore
    /// let _section = log.group("build");                 // header pending…
    /// log.status("compiling x86_64-unknown-linux-gnu");  //    Building binaries
    ///                                                     //    • compiling …
    /// // section closes here as `_section` drops
    /// ```
    #[must_use = "the section stays open only while the guard is alive"]
    pub fn group(&self, title: &str) -> SectionGuard {
        // Defer the header: push it onto the pending stack at the CURRENT depth
        // (before incrementing) and print it only when this section actually
        // emits a body line via `flush_pending`. A stage that does nothing
        // therefore prints no header at all.
        let (verb, msg) = self.split_header(title);
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        pending.push(PendingHeader {
            depth: current_depth(),
            verb: verb.to_string(),
            msg: msg.to_string(),
            flushed: false,
        });
        // Track depth even at Quiet verbosity so any line that DOES print
        // (errors) indents correctly and the guard's decrement is balanced.
        SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
        SectionGuard { _private: () }
    }

    /// Split a stage's [`stage_header`] phrase into the `(verb, message)`
    /// pair [`Self::group`] feeds to [`Self::step`]. The verb is everything
    /// up to the first space; the message is the remainder (empty for a
    /// single-word phrase, which renders as a bare gutter verb). An unknown
    /// stage (default `"Running"`) takes the stage name itself as the
    /// message, so it reads `   Running myfancystage`.
    pub(super) fn split_header<'a>(&self, title: &'a str) -> (&'a str, &'a str) {
        let phrase = stage_header(title);
        match phrase.split_once(' ') {
            Some((verb, rest)) => (verb, rest),
            // Single-word phrase: the default "Running" echoes the stage name
            // as its object; any other single word renders verb-only.
            None if phrase == "Running" => (phrase, title),
            None => (phrase, ""),
        }
    }

    /// Detail message — shown only at Verbose and above. Renders as a `•`
    /// detail body line beneath the current section.
    /// Use for: command output on success, env vars, file paths, template vars.
    pub fn verbose(&self, msg: &str) {
        if self.verbosity >= Verbosity::Verbose {
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(&MARKER_DETAIL.cyan().to_string(), msg)
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Verbose, msg);
        }
    }

    /// Debug message — shown only at Debug level. Renders as a dimmed `•`
    /// detail body line beneath the current section.
    /// Use for: HTTP request/response details, full template contexts, resolved config.
    pub fn debug(&self, msg: &str) {
        if self.verbosity >= Verbosity::Debug {
            flush_pending();
            eprintln!(
                "{}",
                Self::render_body(
                    &MARKER_DETAIL.dimmed().to_string(),
                    &msg.dimmed().to_string()
                )
            );
        }
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Debug, msg);
        }
    }

    /// Emit a per-crate "no `<publisher>` config block" skip line at the
    /// verbosity the operator asked for.
    ///
    /// These lines fire once per non-applicable crate in workspace mode (every
    /// PR-based publisher visits every selected crate and skips the ones whose
    /// config lacks its block), so at default verbosity they would bury the
    /// real output under hundreds of lines of pure no-op noise. They are routed
    /// to [`Self::debug`] (invisible at default and `--verbose`, visible at
    /// `--debug`) unless `show` is set — `--show-skipped`, the diagnostic
    /// escape hatch for "why didn't publisher X run for crate Y?" — in which
    /// case they surface at [`Self::status`] like any other key action.
    pub fn skip_line(&self, show: bool, msg: &str) {
        if show {
            self.status(msg);
        } else {
            self.debug(msg);
        }
    }

    /// Return the current verbosity level.
    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    /// Snapshot this logger's redaction env-pairs (empty when none is
    /// attached). Lets a caller construct a sibling logger at a different
    /// verbosity while preserving the same secret-redaction policy — e.g. the
    /// blob KMS path, which runs its encrypt subprocesses through a
    /// Normal-verbosity clone so ciphertext is never teed live, yet must keep
    /// the original logger's redaction coverage.
    pub fn redaction_env(&self) -> Vec<(String, String)> {
        self.env
            .as_ref()
            .map(|env| env.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    /// Check if verbose output is enabled.
    pub fn is_verbose(&self) -> bool {
        self.verbosity >= Verbosity::Verbose
    }

    /// Check if debug output is enabled.
    pub fn is_debug(&self) -> bool {
        self.verbosity >= Verbosity::Debug
    }

    /// Tee a single line of a child process's standard output to this
    /// process's STDERR, unmodified except for secret redaction.
    ///
    /// Used by the verbose live-stream in [`crate::run::run_checked`]: long
    /// tools (cargo, snapcraft, nix-build) show progress as they run. Unlike
    /// the marker-prefixed body register ([`Self::verbose`]), the line is
    /// written *raw* — no `•` marker, no section indent — so the streamed
    /// output looks exactly as the tool produced it, matching a tee.
    ///
    /// The tee goes to **stderr**, not stdout: anodizer's stdout is a
    /// machine-readable data channel (GHA step outputs like `new_tag=…`, plus
    /// json/changelog/metadata payloads), and a hook's child stdout teed onto
    /// stdout would corrupt it. Progress UX belongs on stderr with every other
    /// human/log line. Recorded into any attached `LogCapture` at
    /// `LogLevel::Verbose` so tests can assert the stream surfaced.
    ///
    /// `line` must be a single line with its trailing newline already
    /// stripped (the caller's line reader does this); the newline is added
    /// here by `eprintln!`.
    pub fn stream_child_stdout(&self, line: &str) {
        self.stream_child_line(line, false);
    }

    /// Tee a single line of a child process's standard error to this
    /// process's stderr, unmodified except for secret redaction. The stderr
    /// companion to [`Self::stream_child_stdout`]; recorded at
    /// `LogLevel::Error` so the verbose-failure double-emit guard is
    /// observable in tests.
    pub fn stream_child_stderr(&self, line: &str) {
        self.stream_child_line(line, true);
    }

    /// Shared body of [`stream_child_stdout`](Self::stream_child_stdout) and
    /// [`stream_child_stderr`](Self::stream_child_stderr): flush any deferred
    /// section header, write the redacted line to stderr, and record it.
    /// Both child streams tee to stderr (see
    /// [`stream_child_stdout`](Self::stream_child_stdout)); the methods differ
    /// only in which capture level the line is recorded under
    /// (`from_stderr` selects `LogLevel::Error` vs `LogLevel::Verbose`),
    /// so the write lives here once.
    ///
    /// `flush_pending()` runs first so a streamed line never prints *above* its
    /// deferred section header — the same header-before-body ordering every
    /// other body-line emitter (`verbose` / `status` / `error` / `debug`)
    /// upholds.
    fn stream_child_line(&self, line: &str, from_stderr: bool) {
        let redacted = self.redact(line);
        flush_pending();
        eprintln!("{redacted}");
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            let level = if from_stderr {
                LogLevel::Error
            } else {
                LogLevel::Verbose
            };
            cap.record(level, redacted);
        }
        #[cfg(not(feature = "test-helpers"))]
        let _ = from_stderr;
    }

    /// Check command output, log stderr/stdout on failure, and bail with context.
    /// On success, log stdout at verbose level. Returns `Ok(output)` on success.
    ///
    /// Stderr and stdout are passed through [`StageLogger::redact`] before
    /// they reach the log sink, so any secret env-var values present in the
    /// subprocess output are replaced with `$KEY_NAME` (and inline
    /// `https://<user>:<pass>@host` URL credentials are scrubbed) without
    /// callers having to remember to redact at each call site. Mirrors
    /// a safe-stderr pattern at every subprocess
    /// boundary.
    pub fn check_output(
        &self,
        output: std::process::Output,
        label: &str,
    ) -> anyhow::Result<std::process::Output> {
        self.check_output_inner(output, label, false)
    }

    /// Like [`StageLogger::check_output`], but for callers that already
    /// streamed the child's stdout/stderr live (the verbose tee in
    /// [`crate::run::run_checked`]). Suppresses the stderr/stdout re-emit
    /// — both on the success-verbose path and the failure path — so output
    /// that was teed line-by-line is not printed a second time. The
    /// `bail!` embed (tail-truncated, redacted stderr in the error chain)
    /// is preserved unchanged, so error-chain consumers still see context.
    pub fn check_output_streamed(
        &self,
        output: std::process::Output,
        label: &str,
    ) -> anyhow::Result<std::process::Output> {
        self.check_output_inner(output, label, true)
    }

    /// Shared body of [`check_output`](Self::check_output) and
    /// [`check_output_streamed`](Self::check_output_streamed).
    ///
    /// `already_streamed` suppresses the stderr/stdout log re-emit (the
    /// caller's live tee already wrote those lines), but never the `bail!`
    /// embed: an error chain propagated past the logger still carries the
    /// redacted, truncated stderr tail regardless of streaming.
    fn check_output_inner(
        &self,
        output: std::process::Output,
        label: &str,
        already_streamed: bool,
    ) -> anyhow::Result<std::process::Output> {
        let (stderr_line, stdout_line) = self.format_output_lines(&output, label);
        if !output.status.success() {
            if !already_streamed {
                if let Some(line) = stderr_line {
                    self.error(&line);
                }
                if let Some(line) = stdout_line {
                    self.error(&line);
                }
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
                // Strip the child's terminal color codes BEFORE redaction and
                // truncation: this tail is bubbled up the anyhow chain and ends
                // up in non-terminal sinks (failure-notification emails, the
                // on_error hook's $ANODIZER_ERROR, JSON run summaries) where raw
                // ANSI renders as garbage around every styled token. Color is
                // forced on for child processes so the live CI log stays
                // colorized; the persisted error must not inherit it. Stripping
                // first also makes the byte cap below count visible content, not
                // escape bytes.
                let stripped = strip_ansi(&stderr_raw);
                let redacted = self.redact(&stripped);
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
        if !already_streamed
            && self.is_verbose()
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
