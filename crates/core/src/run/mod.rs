//! Single subprocess-execution helper that captures stdout/stderr and routes
//! the result through `StageLogger::check_output`.
//!
//! Consolidates the spawn / capture / surface-on-failure pattern that every
//! stage repeats by hand so the success/failure surface stays consistent:
//!
//! - **Default (quiet) verbosity** — the child is captured silently
//!   (`Command::output()`); on success nothing prints, on non-zero exit the
//!   logger emits the redacted stderr/stdout and `bail!`s with a
//!   tail-truncated, redacted stderr tail embedded in the error chain. This
//!   matches GoReleaser's `CombinedOutput()`-then-surface model and produces
//!   zero behavioral drift versus the open-coded `cmd.output()` +
//!   `log.check_output(...)` sites it replaces.
//! - **Verbose / debug** — the child's stdout and stderr are *teed* live to
//!   this process's **stderr** (after secret redaction) AND captured into
//!   in-memory buffers, so a long-running tool (cargo, snapcraft, nix-build,
//!   upx) shows progress as it runs while the failure path keeps the full
//!   captured output for the error embed. The tee deliberately goes to stderr,
//!   never stdout — anodizer's stdout is a machine-readable data channel (GHA
//!   step outputs, JSON payloads) that a teed child stream would corrupt. The
//!   verbose tee is an anodizer-only superset: GoReleaser never streams live.
//!
//! This module does **not** construct a [`std::process::Command`] — that would
//! make `core` a subprocess-spawn surface, which the module-boundary rule
//! forbids. It runs an already-built command supplied by the caller (`&mut
//! Command`), which `module-boundaries.md` explicitly sanctions.
//!
//! Submodules:
//! - `exec` — the `run_checked*` / `run_capture*` entry points and the
//!   spawn / tee / bounded-wait capture loop behind them.
//! - `process_tree` — child-subtree isolation and reaping (Unix process
//!   group, Windows Job Object), the live-subtree registry, and the external
//!   SIGTERM/SIGINT handler that drains it.

mod exec;
mod process_tree;

#[cfg(test)]
mod tests;

pub use exec::{
    run_capture, run_capture_timeout, run_checked, run_checked_timeout, run_checked_with_stdin,
    run_checked_with_stdin_timeout,
};
pub use process_tree::install_termination_handler;
