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
//! Body lines belonging to a [`StageLogger::group`] section additionally
//! carry the section's 2-space nesting indent, so a header's own detail
//! rows sit one level beneath it (the indents in the sketch above are
//! relative, not absolute columns).
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
use std::sync::OnceLock;
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

/// Env var carrying a parent `anodizer` process's visual nesting depth.
///
/// The determinism harness spawns child `anodizer release` subprocesses
/// whose stderr is inherited, so the child's lines interleave directly
/// into the parent's stream. Without an inherited base depth the child's
/// section headers would render flush-left, visually escaping the
/// parent's open section. The parent exports its depth here; the child
/// reads it once (see [`base_depth`]) and offsets every indent by it.
pub const LOG_DEPTH_ENV: &str = "ANODIZER_LOG_DEPTH";

/// Base nesting depth inherited from a parent process via
/// [`LOG_DEPTH_ENV`], parsed once on first use. Zero when the var is
/// absent or unparseable (a standalone process indents from column 0).
static BASE_DEPTH: OnceLock<usize> = OnceLock::new();

/// Parse the inherited base depth from a raw [`LOG_DEPTH_ENV`] value.
/// Lenient by design: a missing or malformed value degrades to 0 (the
/// standalone-process default) rather than failing — indentation is
/// presentation, never worth aborting a release over.
fn parse_base_depth(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// The process's inherited base depth (see [`LOG_DEPTH_ENV`]).
fn base_depth() -> usize {
    *BASE_DEPTH.get_or_init(|| parse_base_depth(std::env::var(LOG_DEPTH_ENV).ok().as_deref()))
}

/// Current absolute nesting depth: the inherited base plus every open
/// section. This is the value [`indent`] renders and the value a parent
/// exports (offset for the child's nesting) when spawning a subprocess
/// whose stderr joins this process's stream.
pub fn current_depth() -> usize {
    base_depth() + SECTION_DEPTH.load(Ordering::Relaxed)
}

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
        eprintln!("{}", render_header(entry.depth, &entry.verb, &entry.msg));
        entry.flushed = true;
    }
}

/// Render a section/stage header line at `depth`: the 2-space-per-level
/// nesting indent, a bold-green verb right-aligned in the [`VERB_COLUMN`]
/// gutter, then (only for a non-empty `msg`) a single space and the message.
///
/// The single source of truth shared by both header-emitting paths — the
/// deferred section header in [`flush_pending`] and the direct
/// [`StageLogger::step`] — so interleaved headers and steps land in
/// byte-identical columns for the same depth. A single-word phrase (empty
/// `msg`) renders the bare gutter verb with no trailing space, so headers
/// never carry stray whitespace.
fn render_header(depth: usize, verb: &str, msg: &str) -> String {
    let prefix = "  ".repeat(depth);
    let verb = format!("{verb:>VERB_COLUMN$}").green().bold();
    if msg.is_empty() {
        format!("{prefix}{verb}")
    } else {
        format!("{prefix}{verb} {msg}")
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
        "check-determinism" => "Checking determinism",
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
    "  ".repeat(current_depth())
}

/// RAII guard returned by [`indent_one_level`]. Removes the extra indent
/// level when dropped.
#[must_use = "dropping the guard immediately removes the extra indent"]
pub struct IndentGuard {
    _private: (),
}

impl Drop for IndentGuard {
    fn drop(&mut self) {
        SECTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Deepen the body indent by one level WITHOUT opening a section header.
///
/// For rows that must align with the body bullets of sibling sections
/// while no section is open — e.g. the pipeline's consolidated
/// `skipped  a, b, c` row, which prints between stage sections (the
/// previous stage's guard has already dropped) but should sit at the
/// same column as those sections' own `•` lines instead of two columns
/// to their left. Unlike [`StageLogger::group`] this pushes no pending
/// header, so nothing extra ever prints.
pub fn indent_one_level() -> IndentGuard {
    SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
    IndentGuard { _private: () }
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
        // Take the PENDING lock BEFORE decrementing the depth: a
        // flush_pending observer on another thread serializes on this
        // lock, so it sees the depth decrement and the pop as one
        // transition instead of a window where the depth is already
        // lowered but the section's pending header is still queued.
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        SECTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
        // Remove this section's pending entry (LIFO matches nesting). An
        // unflushed entry means the section emitted no body line — a no-op
        // stage — so dropping it without printing is exactly the desired
        // "no-op stages print nothing" behavior.
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

    /// Number of [`LogLevel::Debug`] lines recorded.
    pub fn debug_count(&self) -> usize {
        self.count(LogLevel::Debug)
    }

    /// Number of [`LogLevel::Verbose`] lines recorded.
    pub fn verbose_count(&self) -> usize {
        self.count(LogLevel::Verbose)
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
        match self.env.as_deref() {
            Some(env) => crate::redact::with_env(s, env),
            None => crate::redact::redact_url_credentials(s),
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
        self.env.as_deref().cloned().unwrap_or_default()
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
    /// human/log line. Recorded into any attached [`LogCapture`] at
    /// [`LogLevel::Verbose`] so tests can assert the stream surfaced.
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
    /// [`LogLevel::Error`] so the verbose-failure double-emit guard is
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
    /// (`from_stderr` selects [`LogLevel::Error`] vs [`LogLevel::Verbose`]),
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

/// Strip ANSI CSI escape sequences (SGR color codes, cursor moves) from `s`.
///
/// Captured subprocess output (cargo, gpg, …) carries terminal color codes
/// when color is forced for the live CI log. Those bytes must never leak into
/// a propagated error message that reaches a non-terminal sink — a failure
/// email, the `on_error` hook's `$ANODIZER_ERROR`, a JSON run summary — where
/// they render as garbage around every styled token.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Only a CSI introducer (`ESC [`) starts a parameterized sequence
            // we must consume to its final byte (0x40–0x7E); any other escape
            // form (a lone ESC, a two-char sequence) drops just the introducer.
            if chars.next() == Some('[') {
                for ec in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&ec) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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

    /// Run `f` with the process stderr fd (2) redirected to a temp file, then
    /// restore it and return everything that reached fd 2 as a string, or
    /// `None` if `eprintln!` output is being intercepted before fd 2.
    ///
    /// `eprintln!` writes through libtest's macro path, which — under a plain
    /// in-process `cargo test` — diverts output to a thread-local capture sink
    /// BEFORE it reaches fd 2, so an fd swap observes nothing. Under
    /// `cargo nextest` (the CI test runner) each test is its own process with a
    /// real stderr pipe, so the swap captures the true bytes. A sentinel probe
    /// distinguishes the two: if the sentinel does not survive the round-trip,
    /// fd 2 is not the real emit target and the caller must fall back.
    ///
    /// `f` must emit its header as the FIRST line it writes — callers slice the
    /// header off with `.lines().next()`, which is only correct because the
    /// caller's `group()` defers the header and `flush_pending` writes it ahead
    /// of any body line. A change that made `f` emit anything before its header
    /// would silently grab the wrong line.
    ///
    /// Unix-only. The fd-2 swap is process-global across the WHOLE
    /// `anodizer-core` test binary, so callers carry the crate-wide unnamed
    /// `#[serial_test::serial]` key (the convention in
    /// [`crate::test_helpers`]) to mutually exclude against every other
    /// env/cwd/fd-mutating test; `SECTION_TEST_LOCK` only orders the in-file
    /// `PENDING`/`SECTION_DEPTH` state these callers also touch.
    #[cfg(unix)]
    fn capture_stderr(f: impl FnOnce()) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom, Write};
        use std::os::unix::io::AsRawFd;

        /// Restores fd 2 from the saved dup on EVERY exit path, including a
        /// panic in `f` or the probe between the swap and the read. Without
        /// this, an unwind would leave fd 2 pointed at the dropped tempfile, so
        /// every later test's panic/`eprintln!` diagnostics in this shared
        /// process would write to a dangling fd and vanish.
        struct StderrRestore(libc::c_int);
        impl Drop for StderrRestore {
            fn drop(&mut self) {
                // SAFETY: self.0 is the dup of the original stderr taken before
                // the swap; restoring it on every exit path (including unwind)
                // guarantees fd 2 is never left dangling at the tempfile.
                unsafe {
                    libc::dup2(self.0, libc::STDERR_FILENO);
                    libc::close(self.0);
                }
            }
        }

        let mut file = tempfile::tempfile().expect("tempfile for stderr capture");
        std::io::stderr().flush().ok();
        // SAFETY: dup/dup2 on the live stderr fd; the saved fd is owned by the
        // StderrRestore guard below, which restores fd 2 and closes the dup on
        // every exit path (panic-safe). The whole swap is serialized by the
        // caller's crate-wide `#[serial]` key.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved >= 0, "dup(stderr) failed");
        unsafe {
            assert!(
                libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) >= 0,
                "dup2(tempfile, stderr) failed"
            );
        }
        // Owns `saved` from here on; its Drop restores fd 2 even if `f` panics.
        let _restore = StderrRestore(saved);

        const SENTINEL: &str = "__anodizer_capture_probe__";
        eprintln!("{SENTINEL}");
        f();
        std::io::stderr().flush().ok();

        file.seek(SeekFrom::Start(0)).expect("rewind capture file");
        let mut out = String::new();
        file.read_to_string(&mut out).expect("read capture file");
        // The sentinel survives only when fd 2 is the real emit target (nextest
        // / `--nocapture`); under in-process `cargo test` libtest swallowed it
        // (and `f`'s output), so the fd capture cannot prove anything.
        let body = out.strip_prefix(SENTINEL)?.trim_start_matches('\n');
        Some(body.to_string())
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn test_header_paths_emit_identical_bytes() {
        // Regression guard for the v0.9.1 drift where stage headers rendered
        // with 2/3/4/5 leading spaces depending on which path printed them.
        //
        // This drives the TWO REAL emitting paths — the deferred-section header
        // in `flush_pending` and the direct `step` — and asserts they write
        // byte-identical headers at the same depth. It compares ACTUAL stderr
        // bytes (not `render_header`'s return value), so it FAILS the moment
        // either path open-codes its own indent/spacing instead of delegating
        // to `render_header`. Under `cargo nextest` (the CI gate) the fd capture
        // sees real output; under a bare in-process `cargo test` libtest
        // intercepts `eprintln!` and `capture_stderr` returns None, so the body
        // falls back to re-checking the shared helper rather than failing
        // spuriously. The crate-wide `#[serial]` key keeps every other
        // env/fd-mutating test out of the fd-swapped capture; SECTION_TEST_LOCK
        // only orders the in-file PENDING/SECTION_DEPTH state.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("sign", Verbosity::Normal);
        // Absolute depth both paths must render at (includes any inherited base
        // from ANODIZER_LOG_DEPTH, so the anchor below shifts with it).
        let depth = current_depth();

        // flush_pending path: open a section (pending header pushed at `depth`),
        // then a body line triggers `flush_pending`, which prints the header.
        // The header is the FIRST captured line; the body line follows it.
        let flushed = capture_stderr(|| {
            let _section = log.group("sign");
            assert_eq!(
                PENDING.lock().unwrap().last().unwrap().depth,
                depth,
                "pending header must sit at the pre-increment depth"
            );
            log.status("byte-equality probe"); // forces flush_pending
        });
        assert!(
            PENDING.lock().unwrap().is_empty(),
            "guard must pop the entry"
        );

        // step path: the section is closed, so `current_depth()` is back to
        // `depth` — the same depth the pending header rendered at. `step` emits
        // exactly the header line, nothing else.
        assert_eq!(current_depth(), depth, "depth must return to start");
        let stepped = capture_stderr(|| log.step("Signing", "artifacts"));

        let prefix = "  ".repeat(depth);
        let expected = format!("{prefix}{:>VERB_COLUMN$} artifacts", "Signing");

        match (flushed, stepped) {
            (Some(flushed), Some(stepped)) => {
                let flush_header = strip_ansi(
                    flushed
                        .lines()
                        .next()
                        .expect("flush_pending must emit a header line"),
                );
                let step_header = strip_ansi(stepped.trim_end_matches('\n'));
                // The whole point: both REAL paths produce the same header
                // bytes. If a future edit makes one open-code a different
                // indent, these diverge and the test fails.
                assert_eq!(
                    flush_header, step_header,
                    "flush_pending and step must emit byte-identical headers \
                     (flush={flush_header:?} step={step_header:?})"
                );
                // Anchor the shared bytes so a regression that drifts BOTH paths
                // in lockstep (still equal to each other) is also caught.
                assert_eq!(
                    step_header, expected,
                    "header must be indent + gutter verb + space + message"
                );
            }
            // In-process `cargo test`: BOTH swaps were intercepted, so the real
            // paths are unobservable here. Re-assert the shared helper so the
            // test is not a silent no-op; nextest exercises the real bytes.
            (None, None) => {
                assert_eq!(
                    strip_ansi(&render_header(depth, "Signing", "artifacts")),
                    expected
                );
            }
            // The sentinel survived one swap but not the other — a real capture
            // anomaly (a flaky/half-redirected environment), not the documented
            // all-or-nothing fallback. Surface it loudly instead of silently
            // running the weaker check.
            (flushed, stepped) => panic!(
                "inconsistent stderr capture: flush={} step={}",
                flushed.is_some(),
                stepped.is_some()
            ),
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn test_single_word_header_emits_no_trailing_space() {
        // A single-word phrase (empty message) renders the bare gutter verb
        // with NO trailing space on the REAL `step` path — a stray space here
        // would leave invisible whitespace at the end of every `Publishing`
        // header line. Drives `step` directly and inspects the emitted bytes.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("publish", Verbosity::Normal);
        let depth = current_depth();
        let prefix = "  ".repeat(depth);

        match capture_stderr(|| log.step("Publishing", "")) {
            Some(stepped) => {
                let header = strip_ansi(stepped.trim_end_matches('\n'));
                assert_eq!(header, format!("{prefix}{:>VERB_COLUMN$}", "Publishing"));
                assert!(
                    !header.ends_with(' '),
                    "single-word header must not carry a trailing space: {header:?}"
                );
            }
            // In-process `cargo test` intercepts `eprintln!`; re-assert the
            // shared helper so the invariant still has a floor under nextest.
            None => {
                let header = strip_ansi(&render_header(depth, "Publishing", ""));
                assert_eq!(header, format!("{prefix}{:>VERB_COLUMN$}", "Publishing"));
                assert!(!header.ends_with(' '));
            }
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn test_capture_stderr_restores_fd_on_panic() {
        // The fd-restore must run on the unwind path: a panic inside `f`
        // (the asserts in the real callers are a reachable panic path) must
        // not leave fd 2 dangling at the dropped tempfile, which would make
        // every later test's stderr vanish in the shared process.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture_stderr(|| panic!("boom inside capture"));
        }));
        assert!(panicked.is_err(), "the injected panic must propagate");

        // fd 2 is usable again: a fresh capture round-trips its sentinel. (Under
        // in-process `cargo test` the sentinel is swallowed and the result is
        // None — still a successful, non-dangling write; only a leaked fd 2
        // would corrupt this follow-up capture.)
        let after = capture_stderr(|| eprintln!("after panic"));
        if let Some(body) = after {
            assert!(
                body.contains("after panic"),
                "stderr must work after a mid-capture panic: {body:?}"
            );
        }
    }

    #[test]
    fn test_indent_reflects_section_depth() {
        // Indentation tracks the open-section depth (2 spaces per level)
        // identically everywhere — anodizer streams one continuous log, so
        // indentation (not a collapsible `::group::` block) conveys nesting.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("build", Verbosity::Normal);
        // Relative to the inherited base so an exported ANODIZER_LOG_DEPTH
        // in the test environment shifts every expectation uniformly.
        let base = "  ".repeat(base_depth());
        assert_eq!(indent(), base);
        {
            let _outer = log.group("build");
            assert_eq!(indent(), format!("{base}  "));
            {
                let _inner = log.group("sign");
                assert_eq!(indent(), format!("{base}    "));
            }
            assert_eq!(indent(), format!("{base}  "));
        }
        assert_eq!(indent(), base);
    }

    #[test]
    fn test_indent_one_level_adds_depth_without_pending_header() {
        // The header-less guard must deepen the indent (so the row aligns
        // with sibling sections' body bullets) without registering a
        // pending header that a later body line could spuriously flush.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let start = SECTION_DEPTH.load(Ordering::Relaxed);
        let pending_before = PENDING.lock().unwrap().len();
        {
            let _indent = indent_one_level();
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
            assert_eq!(
                PENDING.lock().unwrap().len(),
                pending_before,
                "indent_one_level must not push a pending header"
            );
            assert_eq!(indent(), "  ".repeat(current_depth()));
        }
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
    }

    #[test]
    fn test_parse_base_depth_accepts_valid_and_degrades_invalid() {
        // A subprocess child inherits a numeric depth; anything else
        // (absent, junk, negative) degrades to the standalone default 0 —
        // indentation must never abort a run.
        assert_eq!(parse_base_depth(Some("3")), 3);
        assert_eq!(parse_base_depth(Some(" 2 ")), 2);
        assert_eq!(parse_base_depth(Some("0")), 0);
        assert_eq!(parse_base_depth(Some("-1")), 0);
        assert_eq!(parse_base_depth(Some("abc")), 0);
        assert_eq!(parse_base_depth(Some("")), 0);
        assert_eq!(parse_base_depth(None), 0);
    }

    #[test]
    fn test_current_depth_tracks_sections() {
        // `current_depth` = inherited base (0 in tests — the env var is
        // not set under cargo test) + open sections; it is the value a
        // parent exports to children via LOG_DEPTH_ENV.
        let _guard = SECTION_TEST_LOCK.lock().unwrap();
        let log = StageLogger::new("build", Verbosity::Normal);
        let start = current_depth();
        {
            let _outer = log.group("build");
            assert_eq!(current_depth(), start + 1);
        }
        assert_eq!(current_depth(), start);
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
    fn test_check_output_bail_message_strips_ansi_color_codes() {
        // Color is forced on for child processes so the live CI log stays
        // colorized; cargo (and friends) then emit SGR escapes around versions,
        // paths, and numbers. The bubbled tail flows into failure-notification
        // emails and the on_error hook's $ANODIZER_ERROR, which render raw ANSI
        // as garbage — so the persisted error must carry plain text only.
        let log = StageLogger::new("test", Verbosity::Normal);
        // cargo-shaped colorized stderr: bold version, dimmed path, red error.
        let colorized =
            b"\x1b[1mPackaging\x1b[0m foo \x1b[2mv\x1b[1m0.11.3\x1b[0m\n\x1b[31merror\x1b[0m: exit \x1b[33m101\x1b[0m\n";
        let output = fake_output(b"", colorized, 101);
        let err = log
            .check_output(output, "cargo publish")
            .expect_err("non-zero exit should bail");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains('\u{1b}'),
            "bail message must contain no ANSI escape bytes: {msg:?}"
        );
        assert!(
            msg.contains("Packaging") && msg.contains("0.11.3") && msg.contains("101"),
            "plain-text content must survive ANSI stripping: {msg}"
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
        // Relative to the live indent so an exported ANODIZER_LOG_DEPTH
        // (or a section left open by a parallel test) cannot skew the
        // absolute column.
        let prefix = indent();
        assert_eq!(
            strip(StageLogger::render_body(MARKER_DETAIL, "x")),
            format!("{prefix}   • x")
        );
        assert_eq!(
            strip(StageLogger::render_body(MARKER_SUCCESS, "ok")),
            format!("{prefix}   ✓ ok")
        );
        assert_eq!(
            strip(StageLogger::render_body(MARKER_FAILURE, "bad")),
            format!("{prefix}   ✗ bad")
        );
    }

    #[test]
    fn test_kv_pads_plain_key_so_values_align() {
        // The padded key width counts the PLAIN key, not the ANSI-dimmed
        // bytes, so a short key and a long key share the same value column.
        // Emitting a body line drains the process-global PENDING stack via
        // `flush_pending`, so serialize against the section-depth tests that
        // assert on that stack.
        let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        // Emitting body lines drains the global PENDING stack via
        // `flush_pending`; serialize against the section-depth tests.
        let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn skip_line_records_debug_when_not_shown() {
        // The default (show=false) routes a per-crate "no config block" skip to
        // debug() so it stays invisible at Normal/Verbose and only surfaces at
        // --debug — the fix for the 300+-line workspace skip-noise problem.
        // skip_line emits a body line that drains the global PENDING stack;
        // serialize against the section-depth tests.
        let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (log, cap) = StageLogger::with_capture("homebrew", Verbosity::Normal);
        log.skip_line(
            false,
            "skipped homebrew for crate 'demo' — no homebrew config block",
        );
        assert_eq!(cap.debug_count(), 1);
        assert_eq!(cap.status_count(), 0);
        assert_eq!(
            cap.all_messages(),
            vec![(
                LogLevel::Debug,
                "skipped homebrew for crate 'demo' — no homebrew config block".to_string()
            )]
        );
    }

    #[test]
    fn skip_line_records_status_when_shown() {
        // --show-skipped (show=true) forces the skip line back to status so the
        // operator can diagnose why a publisher didn't run for a given crate.
        // skip_line emits a body line that drains the global PENDING stack;
        // serialize against the section-depth tests.
        let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (log, cap) = StageLogger::with_capture("homebrew", Verbosity::Normal);
        log.skip_line(
            true,
            "skipped homebrew for crate 'demo' — no homebrew config block",
        );
        assert_eq!(cap.status_count(), 1);
        assert_eq!(cap.debug_count(), 0);
    }
}
