//! Line rendering: the verb gutter, body markers, and the deferred section
//! header that only prints once its section produces output.

use super::depth::{base_depth, current_depth};
use std::sync::Mutex;

use colored::Colorize;

/// A section header that has been opened ([`StageLogger::group`](crate::log::StageLogger::group)) but not yet
/// printed. The header line is deferred until the section actually emits a
/// body line, so a stage that does nothing prints nothing at all (matching
/// GoReleaser, which only prints a section header once the section has output).
pub(super) struct PendingHeader {
    /// Section depth captured at open time. The header renders at *this* depth,
    /// not the current global depth, so a nested section's deferred header is
    /// still indented to its own level when flushed alongside its ancestors.
    pub(super) depth: usize,
    /// Right-aligned bold-green verb (the leading word of the stage's
    /// [`stage_header`] phrase).
    pub(super) verb: String,
    /// The remaining words of the phrase, printed after the verb (empty for a
    /// single-word phrase, which renders a bare gutter verb).
    pub(super) msg: String,
    /// Whether this header has already been printed. A flushed entry stays on
    /// the stack (so the LIFO pop in [`SectionGuard::drop`](crate::log::SectionGuard::drop) removes the right
    /// one) but is never reprinted.
    pub(super) flushed: bool,
}

/// Stack of section headers awaiting their first body line. Pushed by
/// [`StageLogger::group`](crate::log::StageLogger::group), drained by [`flush_pending`] when a real line is
/// about to print, and popped (LIFO) by [`SectionGuard::drop`](crate::log::SectionGuard::drop).
///
/// A `Mutex` (not a thread-local) because sections are opened on the main
/// thread like [`SECTION_DEPTH`](super::depth::SECTION_DEPTH), but body lines may flush from a stage's
/// worker threads (e.g. `build` spawning per-target threads). The lock
/// serializes the flush state transition: each header's `flushed` flag flips
/// under the lock, so a header prints exactly once — never lost, never
/// duplicated. Header-before-body ordering holds because every emit method
/// calls [`flush_pending`] then writes its body line with no early return
/// between. It does not serialize body-line-vs-body-line ordering across
/// workers — two `build` threads may print their body lines in either order
/// under a just-flushed header, matching build's existing unordered parallel
/// output.
pub(super) static PENDING: Mutex<Vec<PendingHeader>> = Mutex::new(Vec::new());

/// Print every still-unflushed pending section header, in ancestor-first
/// (bottom-to-top) order, then mark each flushed.
///
/// Called immediately before any method actually writes a visible body line,
/// so the deferred headers appear above their first line in correct nesting
/// order. A header renders at its own stored [`PendingHeader::depth`] — the
/// 2-space-per-level indent, the right-aligned bold-green verb in the
/// `VERB_COLUMN` gutter, then (if non-empty) one space and the message.
///
/// No-op when nothing is pending (the common case once a section has already
/// emitted its first line), so the per-body-line cost is one uncontended lock.
pub(super) fn flush_pending() {
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
/// nesting indent, a bold-green verb right-aligned in the `VERB_COLUMN`
/// gutter, then (only for a non-empty `msg`) a single space and the message.
///
/// The single source of truth shared by both header-emitting paths — the
/// deferred section header in [`flush_pending`] and the direct
/// [`StageLogger::step`](crate::log::StageLogger::step) — so interleaved headers and steps land in
/// byte-identical columns for the same depth. A single-word phrase (empty
/// `msg`) renders the bare gutter verb with no trailing space, so headers
/// never carry stray whitespace.
pub(super) fn render_header(depth: usize, verb: &str, msg: &str) -> String {
    render_gutter(&"  ".repeat(depth), verb, |s| s.green().bold(), msg)
}

/// Render one gutter line: `label` right-aligned in the `VERB_COLUMN` gutter
/// (painted by `paint`) after `prefix`, then a space and `msg` — or, when `msg`
/// is empty, the bare painted label with no trailing space. The single column
/// system shared by section headers and the Warning/Error/Note status labels,
/// so a status label's message lands in the same column as a header's.
pub(super) fn render_gutter(
    prefix: &str,
    label: &str,
    paint: impl Fn(String) -> colored::ColoredString,
    msg: &str,
) -> String {
    let label = paint(format!("{label:>VERB_COLUMN$}"));
    if msg.is_empty() {
        format!("{prefix}{label}")
    } else {
        format!("{prefix}{label} {msg}")
    }
}

/// Width of the right-aligned verb column in [`StageLogger::step`](crate::log::StageLogger::step),
/// matching Cargo's `   Compiling foo` look (3 leading spaces + 9-char
/// verb = a 12-column gutter before the message).
pub(super) const VERB_COLUMN: usize = 12;

/// Indent (after any section nesting) of a body line — a [`StageLogger::detail`](crate::log::StageLogger::detail)
/// / [`success`] / [`failure`] / [`kv`] row, or a status label. Three spaces
/// place the marker column one stop in from the section header's text, so body
/// lines read as subordinate to the header above them.
///
/// [`success`]: crate::log::StageLogger::success
/// [`failure`]: crate::log::StageLogger::failure
/// [`kv`]: crate::log::StageLogger::kv
pub(super) const BODY_INDENT: &str = "   ";

/// Marker for an info / detail body line (`•`). Rendered cyan.
pub(super) const MARKER_DETAIL: &str = "•";

/// Marker for a success body line (`✓`). Rendered green.
pub(super) const MARKER_SUCCESS: &str = "✓";

/// Marker for a failure body line (`✗`). Rendered red.
pub(super) const MARKER_FAILURE: &str = "✗";

/// Map a pipeline stage name to its full Cargo-style header phrase
/// (`"Building binaries"`, `"Signing artifacts"`, `"Publishing"`). Drives
/// [`StageLogger::group`](crate::log::StageLogger::group)'s deferred header: the leading verb is right-aligned
/// into the `VERB_COLUMN` gutter (bold-green, matching `cargo`'s
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
        "install-script" => "Building install script",
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

/// Render the themed `Warning` line for `msg`: the label right-aligned in the
/// gutter (bold-yellow, no colon) at the enclosing section's header indent
/// (`label_indent`), so it reads as a peer of the section verbs and its
/// message aligns with theirs. The single source of truth for the warning
/// palette and label, shared by [`StageLogger::warn`](crate::log::StageLogger::warn) and the CLI's tracing
/// formatter so a library-side `warn!` looks identical to a logger warn (one
/// output authority).
pub fn render_warning(msg: &str) -> String {
    render_gutter(&label_indent(), "Warning", |s| s.yellow().bold(), msg)
}

/// Render the themed `Error` line for `msg` — gutter-aligned bold-red label,
/// companion to [`render_warning`]; shared so the error palette/label lives in
/// exactly one place.
pub fn render_error(msg: &str) -> String {
    render_gutter(&label_indent(), "Error", |s| s.red().bold(), msg)
}

/// Render the themed `Note` line for `msg`: gutter-aligned bold-green label.
/// The third status label — informational lines that are neither warnings nor
/// errors (host-target selection, auto-snapshot activation). The bold-green
/// deliberately matches the section-verb palette (not yellow/red): a note is a
/// neutral peer of the section headers, not an alert. Shared so the `Note`
/// palette/label lives in exactly one place rather than being open-coded per
/// call site.
pub fn render_note(msg: &str) -> String {
    render_gutter(&label_indent(), "Note", |s| s.green().bold(), msg)
}

/// Current indentation prefix (2 spaces per open section). Empty at the
/// top level. Applied identically everywhere — including under GitHub
/// Actions, where the indentation (not a collapsible `::group::` block) is
/// what conveys section nesting, matching the continuous single-stream log
/// GoReleaser emits.
///
/// This is the body-line depth (`•` detail / `success` / `failure` / `kv`
/// rows). Status labels do NOT use it — they sit one level shallower at the
/// enclosing header's depth via `label_indent`.
pub fn indent() -> String {
    "  ".repeat(current_depth())
}

/// Indentation prefix for a status label (`Warning` / `Error` / `Note`).
///
/// A label is a body-level event, but unlike a `•` detail line it renders
/// through the right-aligned [`render_gutter`] system (like a section header),
/// so to land its label in the verb column and its message in the header's
/// message column it must sit at the ENCLOSING header's depth — one level
/// shallower than [`indent`] (which tracks the deeper body depth). Using the
/// body indent here would push the gutter-aligned label two columns past both
/// the sibling headers and the body bullets, leaving it floating on its own.
///
/// Floored at the inherited `base_depth` so a label emitted with no open
/// section never dedents below the ambient (e.g. GitHub Actions) indent.
pub(super) fn label_indent() -> String {
    "  ".repeat(current_depth().saturating_sub(1).max(base_depth()))
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
