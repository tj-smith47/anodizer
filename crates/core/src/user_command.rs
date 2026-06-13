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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    /// Collect the `Command`'s configured env overrides into a map. A
    /// `None` value means the key is explicitly removed; `Some(v)` means
    /// it is set to `v`. After `env_clear`, an unset whitelist key never
    /// appears at all (no override entry is added in the loop above).
    fn env_map(cmd: &Command) -> HashMap<OsString, Option<OsString>> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect()
    }

    #[test]
    fn empty_argv_is_rejected() {
        let argv: &[&str] = &[];
        let err = whitelisted(argv).unwrap_err();
        assert!(
            err.to_string().contains("argv cannot be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn single_element_argv_sets_program_with_no_args() {
        let cmd = whitelisted(&["echo"]).expect("single-element argv is valid");
        assert_eq!(cmd.get_program(), OsStr::new("echo"));
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn multi_element_argv_splits_program_and_args() {
        let cmd =
            whitelisted(&["git", "tag", "-a", "v1.0.0"]).expect("multi-element argv is valid");
        assert_eq!(cmd.get_program(), OsStr::new("git"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![OsStr::new("tag"), OsStr::new("-a"), OsStr::new("v1.0.0")]
        );
    }

    #[test]
    fn whitelisted_env_is_inherited_non_whitelisted_is_dropped() {
        // SAFETY: single-threaded test mutating process env; no other
        // thread reads these keys concurrently within this test binary.
        unsafe {
            std::env::set_var("PATH", "/sentinel/bin");
            std::env::set_var("ANODIZER_SECRET_TOKEN", "leak-me");
        }
        let cmd = whitelisted(&["true"]).expect("valid argv");
        let envs = env_map(&cmd);

        // A whitelisted key present in the parent env is forwarded verbatim.
        assert_eq!(
            envs.get(OsStr::new("PATH")),
            Some(&Some(OsString::from("/sentinel/bin"))),
            "PATH should be inherited from the whitelist"
        );
        // A non-whitelisted key (credential-shaped) must not leak through.
        assert!(
            !envs.contains_key(OsStr::new("ANODIZER_SECRET_TOKEN")),
            "non-whitelisted env must be dropped, got: {envs:?}"
        );

        unsafe {
            std::env::remove_var("ANODIZER_SECRET_TOKEN");
        }
    }

    #[test]
    fn unset_whitelist_key_adds_no_override_entry() {
        // SAFETY: see above — single-threaded env mutation in test context.
        unsafe {
            std::env::remove_var("USERPROFILE");
        }
        let cmd = whitelisted(&["true"]).expect("valid argv");
        let envs = env_map(&cmd);
        // An unset whitelist key is skipped entirely (the loop only adds an
        // override when `std::env::var` returns `Ok`), so it must not appear
        // even as a removal entry.
        assert!(
            !envs.contains_key(OsStr::new("USERPROFILE")),
            "unset whitelist key should add no override entry"
        );
    }
}
