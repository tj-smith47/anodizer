//! `cargo` invocations needed by the CLI's `tag` command.
//!
//! Centralised here so all `Command::new("cargo")` shell-outs live inside
//! the module-boundaries allow-list. `tag.rs` previously called
//! `Command::new("cargo")` from inside the CLI crate — that was outside
//! the allow-list and counted as a boundary violation.

use std::io;
use std::path::Path;
use std::process::Command;

/// Run `cargo update --workspace`, optionally inside `dir`.
///
/// `Ok(true)` — `cargo` ran and exited zero (lockfile updated).
/// `Ok(false)` — `cargo` ran but exited non-zero (e.g. registry network
///   failure, locked-flag conflict). The lockfile may be unchanged.
/// `Err(_)` — `cargo` could not be spawned (missing binary, permission
///   denied, ...). Distinct from `Ok(false)` so callers can log the
///   underlying `io::Error` at trace level if they want to surface why
///   the probe itself failed.
pub fn cargo_update_workspace(dir: Option<&Path>) -> io::Result<bool> {
    let mut cmd = Command::new("cargo");
    cmd.args(["update", "--workspace"]);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.output().map(|o| o.status.success())
}
