//! Spawn a user-supplied command (e.g. `publisher.cmd`) with a clean,
//! whitelisted environment.
//!
//! Centralised here so the `Command::new(<arbitrary>)` shell-out lives
//! inside the module-boundaries allow-list. Inlining this in the CLI
//! crate would put `Command::new` outside the allow-list and counts
//! as a boundary violation.

use std::ffi::OsStr;
use std::process::Command;

/// Environment variables that are inherited from the parent process
/// when constructing a sandboxed `Command`. Anything else must be
/// explicitly added via `Command::env`.
///
/// This whitelist exists to prevent accidental leakage of release
/// credentials (`GITHUB_TOKEN`, `COSIGN_*`, signing keys, etc.) into
/// arbitrary user-supplied commands.
pub const ENV_WHITELIST: &[&str] = &[
    "HOME",
    "USER",
    "USERPROFILE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "PATH",
    "SYSTEMROOT",
];

/// Construct a `Command` whose argv is `argv` and whose environment is
/// reset to the [`ENV_WHITELIST`] subset of the parent's env. The first
/// element of `argv` is the program; the rest are arguments. The caller
/// is responsible for adding any further env vars / cwd / I/O config
/// before invoking `output()`.
///
/// Panics: returns an empty `Command` (program = empty string) when
/// `argv` is empty; callers should reject that case before reaching
/// this helper. The CLI's publisher command does so explicitly.
pub fn whitelisted<S: AsRef<OsStr>>(argv: &[S]) -> Command {
    let program = argv.first().map(AsRef::as_ref).unwrap_or(OsStr::new(""));
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
    cmd
}
