//! Generic external-tool detection for the CLI's `healthcheck` command.
//!
//! Centralised here so the `Command::new(<tool>)` probe shell-outs live
//! inside the module-boundaries allow-list
//! (`.claude/rules/module-boundaries.md`). The CLI used to do these probes
//! inline; that put `Command::new` outside the allow-list and counted as a
//! boundary violation.

use std::io;
use std::process::Command;

/// Probe `<name> --version` and report whether the tool ran successfully.
///
/// `Ok(true)` — `<name> --version` ran and exited zero (tool available).
/// `Ok(false)` — `<name>` ran but exited non-zero (installed but failing
///   `--version`; rare, but possible for stub binaries or version-flag
///   mismatches).
/// `Err(_)` — `<name>` could not be spawned (typically `NotFound` —
///   the binary is not on `PATH`). Distinct from `Ok(false)` so callers
///   can log the underlying `io::Error` at trace level. stdout/stderr
///   are silenced so a missing tool doesn't pollute the log.
pub fn tool_available(name: &str) -> io::Result<bool> {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
}

/// Run `<name> --version` and return the first stdout line trimmed.
///
/// `Ok(Some(line))` — tool ran, exited zero, returns the first stdout
///   line trimmed.
/// `Ok(None)` — tool ran but exited non-zero; no version string to
///   report.
/// `Err(_)` — tool could not be spawned. Distinct from `Ok(None)` so
///   callers can log why the probe itself failed at trace level rather
///   than collapsing every failure to "tool missing".
pub fn tool_version(name: &str) -> io::Result<Option<String>> {
    let output = Command::new(name).arg("--version").output()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Some(stdout.lines().next().unwrap_or("").trim().to_string()))
    } else {
        Ok(None)
    }
}
