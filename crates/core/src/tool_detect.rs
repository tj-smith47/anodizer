//! Generic external-tool detection — `<tool> --version` and
//! `<tool> <args>` probes used by the CLI's `healthcheck` command *and*
//! capability probes elsewhere in core (e.g.
//! `signing::gpg_supports_faked_system_time`, which delegates to
//! [`tool_runs_with_args`]).
//!
//! Centralised here so the `Command::new(<tool>)` probe shell-outs live
//! inside the module-boundaries allow-list. The CLI used to do these
//! probes inline; that put `Command::new` outside the allow-list and
//! counted as a boundary violation. Capability probes in other core
//! modules (signing, etc.) delegate here for the same reason.

use std::io;
use std::process::Command;

/// The version flag `<name>` answers with a zero exit. `--version` for
/// almost everything; OpenSSH's `ssh` rejects `--version` (exit 255,
/// usage text) and only supports `-V`; cosign rejects `--version`
/// (exit 1, "unknown flag") and only supports the `version` subcommand.
fn version_flag(name: &str) -> &'static str {
    match name {
        "ssh" => "-V",
        "cosign" => "version",
        _ => "--version",
    }
}

/// Probe `<name> --version` (or the tool's own version flag, see
/// [`version_flag`]) and report whether the tool ran successfully.
///
/// `Ok(true)` — the probe ran and exited zero (tool available).
/// `Ok(false)` — `<name>` ran but exited non-zero (installed but failing
///   the version flag; rare, but possible for stub binaries or
///   version-flag mismatches).
/// `Err(_)` — `<name>` could not be spawned (typically `NotFound` —
///   the binary is not on `PATH`). Distinct from `Ok(false)` so callers
///   can log the underlying `io::Error` at trace level. stdout/stderr
///   are silenced so a missing tool doesn't pollute the log.
pub fn tool_available(name: &str) -> io::Result<bool> {
    Command::new(name)
        .arg(version_flag(name))
        .current_dir(crate::path_util::probe_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
}

/// Run the tool's version probe (see [`version_flag`]) and return the
/// first output line trimmed.
///
/// `Ok(Some(line))` — tool ran, exited zero, returns the first stdout
///   line trimmed (first stderr line when stdout is empty — ssh prints
///   its version to stderr).
/// `Ok(None)` — tool ran but exited non-zero; no version string to
///   report.
/// `Err(_)` — tool could not be spawned. Distinct from `Ok(None)` so
///   callers can log why the probe itself failed at trace level rather
///   than collapsing every failure to "tool missing".
pub fn tool_version(name: &str) -> io::Result<Option<String>> {
    let output = Command::new(name)
        .arg(version_flag(name))
        .current_dir(crate::path_util::probe_dir())
        .output()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.lines().next().unwrap_or("").trim().to_string();
        if !line.is_empty() {
            return Ok(Some(line));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(Some(stderr.lines().next().unwrap_or("").trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Probe whether `<name> <args...>` runs and exits zero.
///
/// Used by capability probes that pass extra flags beyond bare
/// `--version` (e.g. `gpg --faked-system-time 0! --version` to check
/// whether the local gpg supports deterministic-timestamp signing).
/// stdout/stderr are silenced; `false` covers both "binary missing"
/// and "exited non-zero" — callers that need to distinguish those two
/// cases should use [`tool_available`] / [`tool_version`] instead.
pub fn tool_runs_with_args(name: &str, args: &[&str]) -> bool {
    Command::new(name)
        .args(args)
        .current_dir(crate::path_util::probe_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `true(1)` is part of GNU coreutils on Linux/macOS; it accepts no
    /// args and always exits zero. The test asserts the happy path of
    /// `tool_runs_with_args` does not regress.
    #[test]
    #[cfg(unix)]
    fn tool_runs_with_args_returns_true_for_existing_zero_exit_tool() {
        assert!(tool_runs_with_args("true", &[]));
    }

    /// A binary that definitively does not exist on PATH must collapse
    /// to `false` (not panic, not return `Err`) — the public contract
    /// is "Err and exit-non-zero both fold to false".
    #[test]
    fn tool_runs_with_args_returns_false_for_missing_binary() {
        assert!(!tool_runs_with_args(
            "nonexistent-binary-xyzzy",
            &["--version"]
        ));
    }

    /// ssh must probe with `-V` — OpenSSH exits 255 on `--version`, so
    /// the default flag would report an installed ssh as missing.
    #[test]
    fn version_flag_maps_ssh_to_dash_v() {
        assert_eq!(version_flag("ssh"), "-V");
        assert_eq!(version_flag("git"), "--version");
    }
}
