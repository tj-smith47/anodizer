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
//! - **Status labels** (`Warning` / `Error` / `Note`, via
//!   [`render_warning`] / [`render_error`] / [`render_note`]) render the label
//!   right-aligned in the same verb gutter as a section header (no colon), so
//!   their messages align with the header messages above them.
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
//!
//! Submodules:
//! - `depth` — the process-global section nesting depth and the RAII guards
//!   that move it.
//! - `render` — the verb gutter, the body markers, and the deferred section
//!   header that prints only once its section produces output.
//! - `stage_logger` — [`StageLogger`] itself: the per-stage emitter, its
//!   redaction policy, and the subprocess output check.
//! - `verbosity` — the [`Verbosity`] ladder derived from CLI flags.
//! - `capture` — in-memory capture for tests, behind `test-helpers`.

use std::sync::Arc;
use std::sync::Mutex;

mod capture;
mod depth;
mod render;
mod stage_logger;
mod verbosity;

#[cfg(test)]
mod tests;

#[cfg(feature = "test-helpers")]
pub use capture::{LogCapture, LogLevel};
pub use depth::{IndentGuard, LOG_DEPTH_ENV, SectionGuard, current_depth, indent_one_level};
pub use render::{indent, render_error, render_note, render_warning, stage_header};

pub use stage_logger::StageLogger;
pub use verbosity::Verbosity;

/// Secret-redaction table shape shared by [`StageLogger`] and
/// [`crate::context::Context`]: a live, shareable cell of `(env-var name,
/// value)` pairs. Named so the field/parameter declarations that use it
/// don't repeat the nested `Arc<Mutex<Vec<(String, String)>>>` shape (which
/// clippy's `type_complexity` lint flags on the raw form).
pub(crate) type RedactionEnv = Arc<Mutex<Vec<(String, String)>>>;
