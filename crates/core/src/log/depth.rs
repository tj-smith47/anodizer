//! Process-global section nesting depth and the RAII guards that move it.

use super::render::PENDING;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Process-global section nesting depth. Drives the 2-space-per-level
/// indentation applied to every stderr log line so output produced
/// inside a [`StageLogger::group`](crate::log::StageLogger::group) sits visually beneath its header.
///
/// A single atomic (rather than per-logger state) is correct because the
/// release pipeline drives one stderr stream and no `group()` is ever
/// opened from a worker thread — sections bracket whole stages on the
/// main thread, while a stage's interior parallelism (e.g. `build`
/// spawning per-target threads) emits *inside* an already-open section.
/// The depth is therefore a property of "where the main thread is in the
/// run", not of any individual logger clone or worker.
pub(super) static SECTION_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Env var carrying a parent `anodizer` process's visual nesting depth.
///
/// The determinism harness spawns child `anodizer release` subprocesses
/// whose stderr is inherited, so the child's lines interleave directly
/// into the parent's stream. Without an inherited base depth the child's
/// section headers would render flush-left, visually escaping the
/// parent's open section. The parent exports its depth here; the child
/// reads it once (see `base_depth`) and offsets every indent by it.
pub const LOG_DEPTH_ENV: &str = "ANODIZER_LOG_DEPTH";

/// Base nesting depth inherited from a parent process via
/// [`LOG_DEPTH_ENV`], parsed once on first use. Zero when the var is
/// absent or unparseable (a standalone process indents from column 0).
pub(super) static BASE_DEPTH: OnceLock<usize> = OnceLock::new();

/// Parse the inherited base depth from a raw [`LOG_DEPTH_ENV`] value.
/// Lenient by design: a missing or malformed value degrades to 0 (the
/// standalone-process default) rather than failing — indentation is
/// presentation, never worth aborting a release over.
pub(super) fn parse_base_depth(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// The process's inherited base depth (see [`LOG_DEPTH_ENV`]).
pub(super) fn base_depth() -> usize {
    *BASE_DEPTH.get_or_init(|| parse_base_depth(std::env::var(LOG_DEPTH_ENV).ok().as_deref()))
}

/// Current absolute nesting depth: the inherited base plus every open
/// section. This is the value [`indent`](crate::log::indent) renders and the value a parent
/// exports (offset for the child's nesting) when spawning a subprocess
/// whose stderr joins this process's stream.
pub fn current_depth() -> usize {
    base_depth() + SECTION_DEPTH.load(Ordering::Relaxed)
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
/// to their left. Unlike [`StageLogger::group`](crate::log::StageLogger::group) this pushes no pending
/// header, so nothing extra ever prints.
pub fn indent_one_level() -> IndentGuard {
    SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
    IndentGuard { _private: () }
}

/// RAII guard returned by [`StageLogger::group`](crate::log::StageLogger::group). Closes the section
/// (decrements the indent depth) when dropped, so a stage's body
/// indentation is always balanced even if the stage bails early with `?`.
#[must_use = "dropping the guard immediately ends the section"]
pub struct SectionGuard {
    pub(super) _private: (),
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
