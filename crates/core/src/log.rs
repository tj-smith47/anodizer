//! Thin structured logging helper for anodizer stages.
//!
//! Provides level-gated output to stderr in a single unified style ("format
//! B"). Keeps stdout clean for machine-parseable output (e.g. `anodizer tag`).
//!
//! # Output style
//!
//! Two visual registers, one source of truth — never hand-format a stage line:
//!
//! ```text
//!  Checking determinism          ← SECTION HEADER (group / step)
//!    • targets  aarch64-…         ← META key/value row     (kv)
//!    • stages   build, sign       ← META key/value row     (kv)
//!    • runs     2                 ← META key/value row     (kv)
//!  Building binaries              ← SECTION HEADER
//!    • compiling x86_64-…         ← DETAIL / info line     (detail / status)
//!    ✓ x86_64-…    1.2 MiB         ← SUCCESS line           (success)
//!    ✗ aarch64-…   build failed    ← FAILURE line           (failure)
//! ```
//!
//! - **Section headers** ([`StageLogger::step`] / [`StageLogger::group`]) put a
//!   bold-green present-participle verb (the leading word of the stage's
//!   [`stage_header`] phrase) right-aligned in a fixed 12-column gutter, then
//!   one space, then the message. ONLY verbs live in this gutter — never a
//!   lowercase key.
//! - **Body lines** ([`StageLogger::detail`] / [`success`] / [`failure`], plus
//!   the retargeted [`StageLogger::status`]) sit at a 3-space body indent under
//!   their header, prefixed by a marker — `•` info (cyan), `✓` success (green),
//!   `✗` failure (red) — one space, then the text.
//! - **Key/value rows** ([`StageLogger::kv`]) are `•` detail lines whose
//!   lowercase dimmed key is padded so the values align within a group.
//! - **Status labels** (`Warning:` / `Error:` / `Note:`, via
//!   [`render_warning`] / [`render_error`] / [`render_note`]) render at the body
//!   indent.
//!
//! [`success`]: StageLogger::success
//! [`failure`]: StageLogger::failure
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

/// A section header that has been opened ([`StageLogger::group`]) but not yet
/// printed. The header line is deferred until the section actually emits a
/// body line, so a stage that does nothing prints nothing at all (matching
/// GoReleaser, which only prints a section header once the section has output).
struct PendingHeader {
    /// Section depth captured at open time. The header renders at *this* depth,
    /// not the current global depth, so a nested section's deferred header is
    /// still indented to its own level when flushed alongside its ancestors.
    depth: usize,
    /// Right-aligned bold-green verb (the leading word of the stage's
    /// [`stage_header`] phrase).
    verb: String,
    /// The remaining words of the phrase, printed after the verb (empty for a
    /// single-word phrase, which renders a bare gutter verb).
    msg: String,
    /// Whether this header has already been printed. A flushed entry stays on
    /// the stack (so the LIFO pop in [`SectionGuard::drop`] removes the right
    /// one) but is never reprinted.
    flushed: bool,
}

/// Stack of section headers awaiting their first body line. Pushed by
/// [`StageLogger::group`], drained by [`flush_pending`] when a real line is
/// about to print, and popped (LIFO) by [`SectionGuard::drop`].
///
/// A `Mutex` (not a thread-local) because sections are opened on the main
/// thread like [`SECTION_DEPTH`], but body lines may flush from a stage's
/// worker threads (e.g. `build` spawning per-target threads). The lock
/// serializes the flush state transition: each header's `flushed` flag flips
/// under the lock, so a header prints exactly once — never lost, never
/// duplicated. Header-before-body ordering holds because every emit method
/// calls [`flush_pending`] then writes its body line with no early return
/// between. It does not serialize body-line-vs-body-line ordering across
/// workers — two `build` threads may print their body lines in either order
/// under a just-flushed header, matching build's existing unordered parallel
/// output.
static PENDING: Mutex<Vec<PendingHeader>> = Mutex::new(Vec::new());

/// Print every still-unflushed pending section header, in ancestor-first
/// (bottom-to-top) order, then mark each flushed.
///
/// Called immediately before any method actually writes a visible body line,
/// so the deferred headers appear above their first line in correct nesting
/// order. A header renders at its own stored [`PendingHeader::depth`] — the
/// 2-space-per-level indent, the right-aligned bold-green verb in the
/// [`VERB_COLUMN`] gutter, then (if non-empty) one space and the message.
///
/// No-op when nothing is pending (the common case once a section has already
/// emitted its first line), so the per-body-line cost is one uncontended lock.
fn flush_pending() {
    // Recover a poisoned guard rather than bailing: pending headers are pure
    // presentation state, and silently muting every section header for the
    // rest of the run on one panic-mid-format is worse than reusing it.
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    for entry in pending.iter_mut() {
        if entry.flushed {
            continue;
        }
        let prefix = "  ".repeat(entry.depth);
        let verb = format!("{:>VERB_COLUMN$}", entry.verb).green().bold();
        if entry.msg.is_empty() {
            eprintln!("{prefix}{verb}");
        } else {
            eprintln!("{prefix}{verb} {}", entry.msg);
        }
        entry.flushed = true;
    }
}

/// Width of the right-aligned verb column in [`StageLogger::step`],
/// matching Cargo's `   Compiling foo` look (3 leading spaces + 9-char
/// verb = a 12-column gutter before the message).
const VERB_COLUMN: usize = 12;

/// Indent (after any section nesting) of a body line — a [`StageLogger::detail`]
/// / [`success`] / [`failure`] / [`kv`] row, or a status label. Three spaces
/// place the marker column one stop in from the section header's text, so body
/// lines read as subordinate to the header above them.
///
/// [`success`]: StageLogger::success
/// [`failure`]: StageLogger::failure
/// [`kv`]: StageLogger::kv
const BODY_INDENT: &str = "   ";

/// Marker for an info / detail body line (`•`). Rendered cyan.
const MARKER_DETAIL: &str = "•";

/// Marker for a success body line (`✓`). Rendered green.
const MARKER_SUCCESS: &str = "✓";

/// Marker for a failure body line (`✗`). Rendered red.
const MARKER_FAILURE: &str = "✗";

/// Map a pipeline stage name to its full Cargo-style header phrase
/// (`"Building binaries"`, `"Signing artifacts"`, `"Publishing"`). Drives
/// [`StageLogger::group`]'s deferred header: the leading verb is right-aligned
/// into the [`VERB_COLUMN`] gutter (bold-green, matching `cargo`'s
/// `   Compiling foo` look), and the remaining words form the message that
/// follows. A single-word phrase (`"Publishing"`) renders just the gutter
/// verb with no trailing message.
///
/// The phrase is a *readable description* of the work, not an echo of the
/// stage name — `group("build")` reads `   Building binaries`, not
/// `   Building build`. This keeps the continuous log scannable: a reader
/// sees what each section does, not the internal stage identifier.
///
/// Falls back to `"Running <stage>"` for any stage without a bespoke
/// phrase, so a newly-added stage still renders in the system vocabulary
/// (`   Running myfancystage`) without a code change here.
pub fn stage_header(stage: &str) -> &'static str {
    match stage {
        "setup" => "Preparing release",
        "build" => "Building binaries",
        "archive" => "Creating archives",
        "checksum" => "Computing checksums",
        "sbom" => "Cataloging dependencies",
        "templatefiles" => "Rendering templates",
        "changelog" => "Generating changelog",
        "attest" => "Generating attestations",
        "binary-sign" => "Signing binaries",
        "sign" => "Signing artifacts",
        "docker" => "Building images",
        "docker-sign" => "Signing images",
        "upx" => "Compressing binaries",
        "nfpm" => "Building packages",
        "snapcraft" => "Building snap",
        "flatpak" => "Building Flatpak",
        "msi" => "Building MSI",
        "nsis" => "Building installer",
        "dmg" => "Building DMG",
        "pkg" => "Building pkg",
        "notarize" => "Notarizing app",
        "makeself" => "Building installer",
        "srpm" => "Building source RPM",
        "appbundle" => "Building app bundle",
        "appimage" => "Building AppImage",
        "universal" => "Merging binaries",
        "source" => "Archiving source",
        "release" => "Creating release",
        "before-publish" => "Preparing publishers",
        "emission-validate" => "Validating output",
        "publish" => "Publishing",
        "blob" => "Uploading blobs",
        "snapcraft-publish" => "Publishing snap",
        "announce" => "Announcing release",
        "verify-release" => "Verifying release",
        "publisher-summary" => "Summary",
        "finalize" => "Finalizing",
        "prepare" => "Preparing",
        _ => "Running",
    }
}

/// Render the themed `Warning:` line for `msg`, aligned to the body indent
/// beneath the current section. The single source of truth for the warning
/// palette and label, shared by [`StageLogger::warn`] and the CLI's tracing
/// formatter so a library-side `warn!` looks identical to a logger warn
/// (one output authority).
pub fn render_warning(msg: &str) -> String {
    format!(
        "{}{}{} {}",
        indent(),
        BODY_INDENT,
        "Warning:".yellow().bold(),
        msg
    )
}

/// Render the themed `Error:` line for `msg`, aligned to the body indent
/// beneath the current section. Companion to [`render_warning`]; shared so
/// the error palette/label lives in exactly one place.
pub fn render_error(msg: &str) -> String {
    format!(
        "{}{}{} {}",
        indent(),
        BODY_INDENT,
        "Error:".red().bold(),
        msg
    )
}

/// Render the themed `Note:` line for `msg`, aligned to the body indent
/// beneath the current section. The third (and final) status label in the
/// vocabulary — informational lines that are neither warnings nor errors
/// (host-target selection, auto-snapshot activation). Bold-green to read as
/// a benign status, distinct from the yellow `Warning:` and red `Error:`.
/// Shared so the `Note:` palette/label lives in exactly one place rather than
/// being open-coded per call site.
pub fn render_note(msg: &str) -> String {
    format!(
        "{}{}{} {}",
        indent(),
        BODY_INDENT,
        "Note:".green().bold(),
        msg
    )
}

/// Current indentation prefix (2 spaces per open section). Empty at the
/// top level. Applied identically everywhere — including under GitHub
/// Actions, where the indentation (not a collapsible `::group::` block) is
/// what conveys section nesting, matching the continuous single-stream log
/// GoReleaser emits.
///
/// Exposed so the CLI's loggerless `tracing` warning formatter can apply
/// the same indent a library warn fired mid-stage would otherwise lack,
/// keeping it aligned with the surrounding body lines.
pub fn indent() -> String {
    "  ".repeat(SECTION_DEPTH.load(Ordering::Relaxed))
}

/// RAII guard returned by [`StageLogger::group`]. Closes the section
/// (decrements the indent depth) when dropped, so a stage's body
/// indentation is always balanced even if the stage bails early with `?`.
#[must_use = "dropping the guard immediately ends the section"]
pub struct SectionGuard {
    _private: (),
}

impl Drop for SectionGuard {
    fn drop(&mut self) {
        SECTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
        // Remove this section's pending entry (LIFO matches nesting). An
        // unflushed entry means the section emitted no body line — a no-op
        // stage — so dropping it without printing is exactly the desired
        // "no-op stages print nothing" behavior.
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        pending.pop();
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
    /// The logger's stage identity. No longer printed (the per-line
    /// `[stage]` tag was dropped for the unified body style — section
    /// headers name the stage instead), but retained as the constructor
    /// contract: callers build a logger per stage via [`Self::new`] /
    /// [`crate::context::Context::logger`] and retag sub-sections via
    /// [`Self::with_stage`]. Kept so those entry points keep a stable
    /// signature.
    #[allow(dead_code)]
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

    /// Render a body line: the current section indent, the 3-space body
    /// indent, a colored `marker`, one space, then `text`. The single source
    /// of truth for the `•` / `✓` / `✗` body register so every marker line
    /// aligns byte-identically under its section header.
    fn render_body(marker: &str, text: &str) -> String {
        format!("{}{}{} {}", indent(), BODY_INDENT, marker, text)
    }

    /// Error message — always shown (even in quiet mode). Renders the
    /// `Error:` status label at the body indent beneath the current section.
    pub fn error(&self, msg: &str) {
        flush_pending();
        eprintln!("{}", render_error(msg));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.capture {
            cap.record(LogLevel::Error, msg);
        }
    }

    /// Warning message — shown at Normal and above. Renders the `Warning:`
    /// status label at the body indent beneath the current section.
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

    /// Open a log section for stage `title`.
    ///
    /// The Cargo-style header (derived from [`stage_header`]: the phrase's
    /// leading verb bold-green and right-aligned in the [`VERB_COLUMN`]
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
            depth: SECTION_DEPTH.load(Ordering::Relaxed),
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
    fn split_header<'a>(&self, title: &'a str) -> (&'a str, &'a str) {
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
    /// a safe-stderr pattern at every subprocess
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
        // `group()` increments depth on open and the guard decrements on
        // drop, so nested sections always balance back to the start depth.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
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
        // Even at Quiet verbosity the indent depth must stay balanced so
        // any status lines that DO print (errors) indent correctly and the
        // guard's decrement has a matching increment.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("build", Verbosity::Quiet);
        let start = SECTION_DEPTH.load(Ordering::Relaxed);
        {
            let _s = log.group("build");
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
        }
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
    }

    #[test]
    fn test_group_with_body_flushes_header_once() {
        // A section that emits a real body line flushes its deferred header:
        // the pending entry is marked `flushed` exactly once and stays at its
        // own depth. (`flush_pending` writes the header to stderr; we assert
        // the state transition rather than capture the uncapturable eprintln.)
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("build", Verbosity::Normal);
        {
            let _section = log.group("build");
            // Header is pending, not yet printed.
            assert!(!PENDING.lock().unwrap().last().unwrap().flushed);
            log.status("compiling x86_64-unknown-linux-gnu");
            // The body line flushed the header.
            let pending = PENDING.lock().unwrap();
            let entry = pending.last().unwrap();
            assert!(entry.flushed, "body line must flush the header");
            assert_eq!(entry.verb, "Building");
            assert_eq!(entry.msg, "binaries");
        }
        // Guard drop popped the (flushed) entry.
        assert!(PENDING.lock().unwrap().is_empty());
    }

    #[test]
    fn test_noop_group_prints_no_header() {
        // A section that emits NOTHING leaves its pending entry unflushed, and
        // the guard drop pops it without ever printing — a no-op stage shows
        // no bare header (the GoReleaser behavior). A blank `status("")` spacer
        // is NOT a real body line, so it does not flush either.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("verify-release", Verbosity::Normal);
        {
            let _section = log.group("verify-release");
            log.status(""); // blank spacer — must not flush
            assert!(
                !PENDING.lock().unwrap().last().unwrap().flushed,
                "a no-op section's header must stay unflushed"
            );
        }
        assert!(PENDING.lock().unwrap().is_empty());
    }

    #[test]
    fn test_nested_groups_flush_in_ancestor_order() {
        // A body line in a nested section flushes BOTH the ancestor and the
        // nested header (each at its own stored depth), so the deferred
        // headers appear in correct order above the first line.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("publish", Verbosity::Normal);
        let start = SECTION_DEPTH.load(Ordering::Relaxed);
        {
            let _outer = log.group("publish");
            {
                let _inner = log.group("blob");
                log.status("uploading blob");
                let pending = PENDING.lock().unwrap();
                assert_eq!(pending.len(), 2);
                assert!(pending[0].flushed, "ancestor header must flush");
                assert!(pending[1].flushed, "nested header must flush");
                assert_eq!(pending[0].depth, start);
                assert_eq!(pending[1].depth, start + 1);
            }
        }
        assert!(PENDING.lock().unwrap().is_empty());
    }

    #[test]
    fn test_indent_reflects_section_depth() {
        // Indentation tracks the open-section depth (2 spaces per level)
        // identically everywhere — anodizer streams one continuous log, so
        // indentation (not a collapsible `::group::` block) conveys nesting.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("build", Verbosity::Normal);
        assert_eq!(indent(), "");
        {
            let _outer = log.group("build");
            assert_eq!(indent(), "  ");
            {
                let _inner = log.group("sign");
                assert_eq!(indent(), "    ");
            }
            assert_eq!(indent(), "  ");
        }
        assert_eq!(indent(), "");
    }

    #[test]
    fn test_stage_header_splits_into_verb_and_message() {
        // A multi-word phrase splits on the FIRST space: the verb feeds the
        // right-aligned gutter, the remainder is the section message.
        let log = StageLogger::new("build", Verbosity::Normal);
        assert_eq!(log.split_header("build"), ("Building", "binaries"));
        assert_eq!(log.split_header("sign"), ("Signing", "artifacts"));
        assert_eq!(log.split_header("source"), ("Archiving", "source"));
    }

    #[test]
    fn test_stage_header_single_word_renders_verb_only() {
        // A known single-word phrase ("Publishing") renders just the gutter
        // verb with an empty message — no stage-name echo.
        let log = StageLogger::new("publish", Verbosity::Normal);
        assert_eq!(log.split_header("publish"), ("Publishing", ""));
    }

    #[test]
    fn test_stage_header_unknown_stage_uses_running_plus_name() {
        // An unknown stage falls back to "Running" + the stage name, so it
        // still renders in the system vocabulary (`   Running myfancystage`).
        let log = StageLogger::new("x", Verbosity::Normal);
        assert_eq!(
            log.split_header("myfancystage"),
            ("Running", "myfancystage")
        );
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

    #[test]
    fn test_with_stage_rebinds_stage_field() {
        // The per-line `[stage]` tag is gone from rendered output, but
        // `with_stage` still rebinds the `stage` field a logger carries (it
        // drives redaction env inheritance, not line formatting now).
        let log = StageLogger::new("release", Verbosity::Normal);
        assert_eq!(log.stage, "release");
        assert_eq!(log.with_stage("finalize").stage, "finalize");
    }

    #[test]
    fn test_body_markers_render_at_body_indent() {
        // Body lines sit at the 3-space body indent (top level: no section
        // nesting) behind a colored marker glyph. ANSI codes are stripped
        // for the assertion so the test pins the visible shape, not palette.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        // SAFETY: single-threaded under SECTION_TEST_LOCK.
        unsafe {
            std::env::remove_var("GITHUB_ACTIONS");
        }
        let strip = |s: String| {
            // Drop CSI sequences so the assertion is palette-independent.
            let mut out = String::new();
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for n in chars.by_ref() {
                        if n == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(
            strip(StageLogger::render_body(MARKER_DETAIL, "x")),
            "   • x"
        );
        assert_eq!(
            strip(StageLogger::render_body(MARKER_SUCCESS, "ok")),
            "   ✓ ok"
        );
        assert_eq!(
            strip(StageLogger::render_body(MARKER_FAILURE, "bad")),
            "   ✗ bad"
        );
    }

    #[test]
    fn test_kv_pads_plain_key_so_values_align() {
        // The padded key width counts the PLAIN key, not the ANSI-dimmed
        // bytes, so a short key and a long key share the same value column.
        let (log, cap) = StageLogger::with_capture("check", Verbosity::Normal);
        let w = ["targets", "runs"].iter().map(|k| k.len()).max().unwrap();
        log.kv("targets", "aarch64", w);
        log.kv("runs", "2", w);
        // The capture stores a normalized `key = value` form regardless of
        // the rendered padding/palette.
        assert_eq!(
            cap.all_messages(),
            vec![
                (LogLevel::Status, "targets = aarch64".to_string()),
                (LogLevel::Status, "runs = 2".to_string()),
            ]
        );
    }

    #[test]
    fn test_retag_helpers_record_under_shared_capture() {
        // The retagged clone shares the capture sink, and the plain
        // delegations still record at the right level — locking the plumbing
        // independent of the rendered tag (which the capture does not store).
        let (log, cap) = StageLogger::with_capture("release", Verbosity::Normal);

        log.with_stage("finalize").status("x");
        log.error("y");
        log.status("own-status");
        log.error("own-error");

        assert_eq!(
            cap.all_messages(),
            vec![
                (LogLevel::Status, "x".to_string()),
                (LogLevel::Error, "y".to_string()),
                (LogLevel::Status, "own-status".to_string()),
                (LogLevel::Error, "own-error".to_string()),
            ]
        );
    }
}
