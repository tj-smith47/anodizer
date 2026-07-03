//! Generic external-tool detection. The one module answering both
//! availability questions, each right for a different surface:
//!
//! - [`on_path`] — "is `<tool>` reachable on `PATH`?" A pure lookup with no
//!   exec. The right question for **existence gating** (can a stage spawn
//!   this binary): tools with no version flag (`hdiutil`) or that exit
//!   non-zero on `--version` (`pkgbuild`, WiX `candle`/`light`) are present
//!   and runnable yet fail a version probe, so gating on [`runs`] reports
//!   them missing and hard-fails work that would have succeeded.
//! - [`runs`] — "does `<tool> <version-flag>` actually run and exit zero?"
//!   A spawn probe. The right question for **health reporting**
//!   (`healthcheck`, validator availability): a broken stub on `PATH`
//!   passes [`on_path`] but not [`runs`].
//!
//! Also hosts `<tool> <args>` capability probes (e.g.
//! `signing::gpg_supports_faked_system_time`, which delegates to
//! [`tool_runs_with_args`]).
//!
//! Centralised here so the `Command::new(<tool>)` probe shell-outs live
//! inside the module-boundaries allow-list. The CLI used to do these
//! probes inline; that put `Command::new` outside the allow-list and
//! counted as a boundary violation. Capability probes in other core
//! modules (signing, etc.) delegate here for the same reason.

use std::io;
use std::path::Path;
use std::process::Command;

/// Outcome of a spawn-probe availability check ([`runs`]).
///
/// The `NotFound`-folds-into-`Unavailable` decision is made exactly once,
/// here — and a genuine probe failure is a distinct variant so no call site
/// can silently masquerade a broken probe as clean tool absence.
#[derive(Debug)]
pub enum ToolProbe {
    /// The probe ran and exited zero — the tool is available.
    Available,
    /// The tool is cleanly absent: not on `PATH` (spawn failed with
    /// `NotFound`), or it ran but exited non-zero on its version flag
    /// (stub binary / version-flag mismatch).
    Unavailable,
    /// The probe itself failed for a non-`NotFound` reason (permission
    /// denied, exec-format error, …): presence is UNKNOWN, not "absent".
    /// Callers must surface the error rather than collapse it to
    /// [`ToolProbe::Unavailable`].
    ProbeFailed(io::Error),
}

/// Check whether a binary is reachable on the system — a pure `PATH`
/// lookup with no exec.
///
/// For absolute or relative paths (containing `/`), checks if the file
/// exists. For bare names, searches each directory in the `PATH`
/// environment variable for an executable with the given name. This is a
/// pure-Rust implementation that avoids shelling out to `which` or
/// `command -v`, making it portable across all platforms.
pub fn on_path(name: &str) -> bool {
    if name.contains('/') || name.contains('\\') {
        return Path::new(name).exists();
    }

    // On Windows, PATHEXT lists extensions to try (e.g., .COM;.EXE;.BAT;.CMD).
    // When the caller asks for "upx", we also check for "upx.exe", etc.
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_string())
            .collect()
    } else {
        Vec::new()
    };

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
            for ext in &extensions {
                let with_ext = dir.join(format!("{}{}", name, ext));
                if with_ext.is_file() {
                    return true;
                }
            }
        }
    }

    false
}

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
/// [`version_flag`]) and report the tri-state outcome.
///
/// A missing-on-`PATH` binary (spawn `NotFound`) is folded together with a
/// ran-but-exited-non-zero probe into [`ToolProbe::Unavailable`] — the one
/// place that fold happens. Any other spawn error is
/// [`ToolProbe::ProbeFailed`] and must be surfaced by the caller.
/// stdout/stderr are silenced so a missing tool doesn't pollute the log.
pub fn runs(name: &str) -> ToolProbe {
    match Command::new(name)
        .arg(version_flag(name))
        .current_dir(crate::path_util::probe_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => ToolProbe::Available,
        Ok(_) => ToolProbe::Unavailable,
        Err(e) if e.kind() == io::ErrorKind::NotFound => ToolProbe::Unavailable,
        Err(e) => ToolProbe::ProbeFailed(e),
    }
}

/// How many leading output lines [`extract_version_line`] scans for a
/// version-looking line. Bounded so a chatty tool cannot make the probe
/// scan megabytes; comfortably clears cosign's ASCII banner (~10 lines
/// before `GitVersion:`).
const VERSION_SCAN_LINES: usize = 15;

/// Pick the first version-looking line — non-empty and carrying at least
/// one digit — from the leading [`VERSION_SCAN_LINES`] lines of `stdout`,
/// falling back to `stderr` (ssh prints its version there). `None` when
/// neither stream yields one, so callers omit the version instead of
/// rendering banner art (cosign leads with a digit-free ASCII banner that
/// a naive first-line grab would report as its version).
fn extract_version_line(stdout: &str, stderr: &str) -> Option<String> {
    let versionish = |text: &str| {
        text.lines()
            .take(VERSION_SCAN_LINES)
            .map(str::trim)
            .find(|line| !line.is_empty() && line.chars().any(|c| c.is_ascii_digit()))
            .map(str::to_string)
    };
    versionish(stdout).or_else(|| versionish(stderr))
}

/// Run the tool's version probe (see [`version_flag`]) and return a
/// version-looking output line.
///
/// `Ok(Some(line))` — tool ran, exited zero, and one of the leading
///   output lines looks like a version (see [`extract_version_line`]).
/// `Ok(None)` — tool ran but exited non-zero, or produced no
///   version-looking line; no version string to report.
/// `Err(_)` — tool could not be spawned. Distinct from `Ok(None)` so
///   callers can log why the probe itself failed at trace level rather
///   than collapsing every failure to "tool missing".
pub fn tool_version(name: &str) -> io::Result<Option<String>> {
    let output = Command::new(name)
        .arg(version_flag(name))
        .current_dir(crate::path_util::probe_dir())
        .output()?;
    if output.status.success() {
        Ok(extract_version_line(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ))
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
/// cases should use [`runs`] / [`tool_version`] instead.
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

    #[test]
    fn runs_reports_present_tool_available() {
        assert!(matches!(runs("git"), ToolProbe::Available));
    }

    /// The missing-binary case is the CLEAN absence outcome: the
    /// `NotFound`-folds-into-`Unavailable` decision lives in `runs`, not
    /// at every call site.
    #[test]
    fn runs_folds_not_found_into_unavailable() {
        assert!(matches!(
            runs("this-tool-does-not-exist-12345"),
            ToolProbe::Unavailable
        ));
    }

    #[test]
    fn on_path_absolute_path_exists() {
        if cfg!(windows) {
            // cmd.exe exists on all Windows systems
            assert!(on_path("C:\\Windows\\System32\\cmd.exe"));
        } else {
            // /usr/bin/env exists on virtually all Unix systems
            assert!(on_path("/usr/bin/env"));
        }
    }

    #[test]
    fn on_path_absolute_path_does_not_exist() {
        if cfg!(windows) {
            assert!(!on_path("C:\\nonexistent\\binary\\path.exe"));
        } else {
            assert!(!on_path("/nonexistent/binary/path"));
        }
    }

    #[test]
    fn on_path_bare_name_on_path() {
        if cfg!(windows) {
            // "cmd.exe" should be findable on PATH on any Windows system.
            // The extension-qualified form matches directly; bare names are
            // additionally probed with each PATHEXT extension appended, so
            // plain "cmd" would resolve too.
            assert!(on_path("cmd.exe"));
        } else {
            // "env" should be findable on PATH on any Unix system
            assert!(on_path("env"));
        }
    }

    #[test]
    fn on_path_bare_name_not_on_path() {
        assert!(!on_path("nonexistent-binary-xyz-12345"));
    }

    /// cosign leads its `version` output with a digit-free ASCII banner;
    /// the extractor must skip past it to the first version-looking line
    /// instead of reporting banner art as the version.
    #[test]
    fn extract_version_line_skips_cosign_banner() {
        let stdout = [
            "  ______   ______        _______. __    _______ .__   __.",
            " /      | /  __  \\      /       ||  |  /  _____||  \\ |  |",
            "|  ,----'|  |  |  |    |   (----`|  | |  |  __  |   \\|  |",
            "|  `----.|  `--'  | .----)   |   |  | |  |__| | |  |\\   |",
            " \\______| \\______/  |_______/    |__|  \\______| |__| \\__|",
            "cosign: A tool for Container Signing, Verification and Storage in an OCI registry.",
            "",
            "GitVersion:    v2.2.4",
            "GitCommit:     abc",
        ]
        .join("\n");
        assert_eq!(
            extract_version_line(&stdout, ""),
            Some("GitVersion:    v2.2.4".to_string())
        );
    }

    /// The common single-line case ("git version 2.43.0") is unchanged by
    /// the banner-skipping scan.
    #[test]
    fn extract_version_line_takes_normal_first_line() {
        assert_eq!(
            extract_version_line("git version 2.43.0\n", ""),
            Some("git version 2.43.0".to_string())
        );
        // ssh prints its version to stderr.
        assert_eq!(
            extract_version_line("", "OpenSSH_9.6p1, OpenSSL 3.0.13\n"),
            Some("OpenSSH_9.6p1, OpenSSL 3.0.13".to_string())
        );
    }

    /// No digit anywhere in the scanned window → no version to report;
    /// callers omit the parenthetical rather than print garbage.
    #[test]
    fn extract_version_line_returns_none_without_digits() {
        assert_eq!(extract_version_line("all prose, no version\n", ""), None);
        assert_eq!(extract_version_line("", ""), None);
    }
}
