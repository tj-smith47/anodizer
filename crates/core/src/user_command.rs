//! Spawn a user-supplied command (e.g. `publisher.cmd`) with a clean,
//! whitelisted environment.
//!
//! Centralised here so the `Command::new(<arbitrary>)` shell-out lives
//! inside the module-boundaries allow-list. Inlining this in the CLI
//! crate would put `Command::new` outside the allow-list and counts
//! as a boundary violation.

use std::ffi::OsStr;
use std::process::Command;

use anyhow::Result;

/// Environment variables that are inherited from the parent process
/// when constructing a sandboxed `Command`. Anything else must be
/// explicitly added via `Command::env`.
///
/// This whitelist exists to prevent accidental leakage of release
/// credentials (`GITHUB_TOKEN`, `COSIGN_*`, signing keys, etc.) into
/// arbitrary user-supplied commands.
///
/// Includes toolchain vars (`RUSTUP_*`, `CARGO_HOME`) so user hooks
/// invoking `cargo` resolve a toolchain — without them, rustup-managed
/// runners fail with `rustup could not choose a version of cargo to
/// run`. Also includes CI identity vars (`CI`, `GITHUB_*`, `RUNNER_*`)
/// so hooks can detect the runner context and emit appropriate output
/// without seeing release credentials (`GITHUB_TOKEN` is intentionally
/// not in the list).
pub const ENV_WHITELIST: &[&str] = &[
    "HOME",
    "USER",
    "USERPROFILE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "PATH",
    "SYSTEMROOT",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_HOME",
    "CI",
    "GITHUB_ACTIONS",
    "GITHUB_WORKFLOW",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_NUMBER",
    "GITHUB_JOB",
    "GITHUB_REPOSITORY",
    "GITHUB_REF",
    "GITHUB_REF_NAME",
    "GITHUB_SHA",
    "GITHUB_EVENT_NAME",
    "GITHUB_WORKSPACE",
    "RUNNER_OS",
    "RUNNER_ARCH",
    "RUNNER_NAME",
    "RUNNER_TEMP",
    "RUNNER_TOOL_CACHE",
];

/// Construct a `Command` whose argv is `argv` and whose environment is
/// reset to the [`ENV_WHITELIST`] subset of the parent's env. The first
/// element of `argv` is the program; the rest are arguments. The caller
/// is responsible for adding any further env vars / cwd / I/O config
/// before invoking `output()`.
///
/// Returns `Err` when `argv` is empty — surfacing a clear error at the
/// allow-listed boundary is preferable to deferring failure to the
/// kernel via an empty `program` path.
pub fn whitelisted<S: AsRef<OsStr>>(argv: &[S]) -> Result<Command> {
    anyhow::ensure!(!argv.is_empty(), "user command argv cannot be empty");
    let program = argv[0].as_ref();
    let mut cmd = Command::new(program);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.env_clear();
    for key in ENV_WHITELIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    Ok(cmd)
}
