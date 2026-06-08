//! `docker` invocations needed by the CLI's `check` command.
//!
//! Centralised here so all `Command::new("docker")` shell-outs live inside
//! the module-boundaries allow-list.

use std::io;
use std::process::Command;

/// Run `docker buildx version` and return whether buildx is installed and
/// reachable.
///
/// `Ok(true)` — `docker buildx version` ran and exited zero.
/// `Ok(false)` — `docker` ran but the `buildx` subcommand exited non-zero
///   (typically: docker is present but buildx plugin not installed).
/// `Err(_)` — `docker` itself could not be spawned (binary missing,
///   permission denied). Distinct from `Ok(false)` so callers can log the
///   underlying `io::Error` at trace level when surfacing "docker not
///   available" vs. "docker installed but buildx missing".
pub fn buildx_available() -> io::Result<bool> {
    Command::new("docker")
        .args(["buildx", "version"])
        .current_dir(crate::path_util::probe_dir())
        .output()
        .map(|o| o.status.success())
}
