//! Hermetic child-subprocess env construction for the harness.
//!
//! Owns the platform-specific allow-list / deny-list policy:
//!
//! - **Linux / macOS**: explicit identity-only [`HARNESS_ENV_ALLOWLIST`].
//! - **Windows**: inherit everything, then drop credentials, workflow
//!   internals, and `GITHUB_*` / `RUNNER_*` namespace state via
//!   [`windows_env_should_drop`].
//!
//! Plus the per-target MSVC determinism flag injection (`/Brepro` /
//! `/OPT:NOICF` / `/INCREMENTAL:NO` / `/DEBUG:NONE` / strip=symbols),
//! `--remap-path-prefix` for worktree-path elision, and the rustup /
//! signing-keys plumbing.

use anodizer_core::harness_signing::EphemeralSigningKeys;
use std::collections::HashMap;
use std::path::Path;

/// Explicit allow-list of host env vars the harness propagates into the
/// child build subprocess.
///
/// Two policy goals:
///
/// 1. **No credentials leak through.** Earlier shape was
///    `k.starts_with("GITHUB_")`, which would inherit `GITHUB_TOKEN` (the
///    OAuth token) and any future `GITHUB_PASSWORD`-style sibling. The
///    harness skips every token-consuming stage today, so the leak is
///    latent — but a future stage added to the build phase (e.g. a
///    registry-prefetch step) would silently acquire network creds inside
///    a supposedly-hermetic build.
///
/// 2. **Identity, not credentials.** Each entry below is either an
///    informational field (`GITHUB_REPOSITORY`, `RUNNER_OS`) or a build-
///    script identity input (`GITHUB_SHA`, `GITHUB_REF`). Nothing here
///    grants the child process network reach.
///
/// Adding a new var here must be justified as identity-only; cred-bearing
/// vars belong in `crates/core/src/user_command.rs`'s sandboxed env
/// whitelist, not the harness's inheritance set.
///
/// This list is the contractual surface on **all** platforms. On Windows,
/// [`build_subprocess_env`] additionally inherits the rest of the host env
/// minus everything covered by [`windows_env_should_drop`].
pub(super) const HARNESS_ENV_ALLOWLIST: &[&str] = &[
    // Toolchain identity.
    "RUSTUP_HOME",
    // CI signal (overridden to "true" below if unset).
    "CI",
    // GitHub Actions identity vars — owner/repo, commit, refs, run #.
    "GITHUB_REPOSITORY",
    "GITHUB_SHA",
    "GITHUB_REF",
    "GITHUB_REF_NAME",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_NUMBER",
    "GITHUB_WORKFLOW",
    "GITHUB_ACTOR",
    // Runner identity — OS / arch / hostname for build-script `cfg!()`.
    "RUNNER_OS",
    "RUNNER_ARCH",
    "RUNNER_NAME",
];

/// Credential-bearing env vars the Windows inherit-everything pass MUST
/// drop. The explicit list is the contractual surface; the
/// [`windows_env_should_drop`] suffix sweep is the defense-in-depth net
/// for vars not named here.
#[cfg(windows)]
const WINDOWS_ENV_DENYLIST: &[&str] = &[
    // GitHub Actions / generic Git credentials.
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_PAT",
    // Cargo / publish credentials.
    "CARGO_REGISTRY_TOKEN",
    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
    // Cloud / store credentials.
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GCP_SERVICE_ACCOUNT_KEY",
    "AZURE_CLIENT_SECRET",
    // Anodize-publisher credentials.
    "CHOCOLATEY_API_KEY",
    "DOCKER_TOKEN",
    "DOCKERHUB_TOKEN",
    "GPG_PRIVATE_KEY",
    "GPG_PASSPHRASE",
    "COSIGN_KEY",
    "COSIGN_PASSWORD",
    "SNAPCRAFT_STORE_CREDENTIALS",
    "CLOUDSMITH_TOKEN",
    "MCP_GITHUB_TOKEN",
    "SMTP_PASSWORD",
    "ARTIFACTORY_TOKEN",
    "APK_PRIVATE_KEY",
    // Runner workflow-internal.
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_RUNTIME_URL",
    "ACTIONS_CACHE_URL",
    "ACTIONS_RESULTS_URL",
    "RUNNER_TOKEN",
];

/// True when `key` names an env var the Windows inherit-everything pass
/// must drop. The predicate covers two distinct contracts:
///
/// 1. **Credentials / workflow-internal state** — vars whose value grants
///    network reach, signing authority, or store-publishing rights, plus
///    GH Actions workflow internals that pollute the child env.
/// 2. **Hermeticity** — vars in the `GITHUB_*` / `RUNNER_*` namespaces that
///    are NOT on [`HARNESS_ENV_ALLOWLIST`]. The allow-list captures the
///    identity-only subset (repo / sha / refs / run #, os / arch /
///    hostname); the rest of those namespaces is host workflow state
///    (`RUNNER_TEMP`, `RUNNER_TOOL_CACHE`, `RUNNER_WORKSPACE`,
///    `GITHUB_WORKSPACE`, `GITHUB_EVENT_PATH`, ...) — path-pointing or
///    workflow-state values that would leak the GH Actions runner's
///    on-host directories into the supposedly hermetic child.
///
/// Check order: explicit deny-list → ACTIONS_* / RUNNER_TOKEN → credential
/// suffix sweep → GH/RUNNER namespace hermeticity gate.
#[cfg(windows)]
fn windows_env_should_drop(key: &str) -> bool {
    if WINDOWS_ENV_DENYLIST
        .iter()
        .any(|d| d.eq_ignore_ascii_case(key))
    {
        return true;
    }
    if key.starts_with("ACTIONS_") || key.eq_ignore_ascii_case("RUNNER_TOKEN") {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    for suffix in [
        "_token",
        "_key",
        "_secret",
        "_password",
        "_passphrase",
        "_credentials",
    ] {
        if lower.ends_with(suffix) {
            return true;
        }
    }
    if key.starts_with("GITHUB_") || key.starts_with("RUNNER_") {
        let in_allowlist = HARNESS_ENV_ALLOWLIST
            .iter()
            .any(|a| a.eq_ignore_ascii_case(key));
        if !in_allowlist {
            return true;
        }
    }
    false
}

/// Inputs for [`build_subprocess_env`]. Bundled so the function signature
/// doesn't grow more positional arguments every time we add an isolated-
/// path knob.
pub(crate) struct BuildSubprocessEnv<'a> {
    pub cargo_home: &'a Path,
    pub cargo_target: &'a Path,
    pub tmpdir: &'a Path,
    pub home_dir: &'a Path,
    pub sde: i64,
    /// Absolute path to the per-run worktree root. Used to inject
    /// `RUSTFLAGS=--remap-path-prefix=<worktree>=/anodize` into the child
    /// build subprocess so two harness runs (at different worktree paths)
    /// produce a byte-identical anodizer binary.
    pub worktree: &'a Path,
    /// Ephemeral signing keys for the sign stage. `None` skips the
    /// keying env-var block (caller is opting out of sign-stage
    /// validation). When `Some`, the harness exports `COSIGN_KEY` /
    /// `COSIGN_PASSWORD` / `GNUPGHOME` / `GPG_FINGERPRINT` / `GPG_TTY` /
    /// `GPG_KEY_PATH` / `ANODIZER_IN_DETERMINISM_HARNESS=1` into the
    /// child env.
    pub signing_keys: Option<&'a EphemeralSigningKeys>,
}

/// PATH for harness children — inherits the host's PATH verbatim on
/// every platform.
///
/// The harness's hermeticity goal is to isolate cargo/build outputs
/// from the host's CARGO_HOME and HOME (so two runs of the same commit
/// don't share warm caches), NOT to tighten the binary-search path.
/// Two runs from the same host process see identical host PATH, so
/// determinism is preserved.
pub(super) fn allow_listed_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Pure constructor for the child env map.
///
/// Reads from `std::env::vars()` for the allow-listed identity vars (see
/// [`HARNESS_ENV_ALLOWLIST`]). Unit tests that care about the host-env
/// pass-through must serialize on the `harness_env` lock group via
/// `serial_test::serial(harness_env)` — env vars are process-global state
/// and parallel tests racing on the same key cause flakes.
pub(crate) fn build_subprocess_env(inputs: &BuildSubprocessEnv<'_>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "CARGO_HOME".into(),
        inputs.cargo_home.to_string_lossy().into_owned(),
    );
    env.insert(
        "CARGO_TARGET_DIR".into(),
        inputs.cargo_target.to_string_lossy().into_owned(),
    );
    env.insert(
        "TMPDIR".into(),
        inputs.tmpdir.to_string_lossy().into_owned(),
    );
    env.insert(
        "HOME".into(),
        inputs.home_dir.to_string_lossy().into_owned(),
    );
    env.insert("SOURCE_DATE_EPOCH".into(), inputs.sde.to_string());
    env.insert("PATH".into(), allow_listed_path());

    // Inject `--remap-path-prefix` so the absolute worktree path doesn't
    // leak into the compiled binary. Rustc embeds the absolute workspace
    // path into every `file!()` / `Location::caller()` expansion (panic
    // location strings, `#[track_caller]` slots, line-tables-only debug
    // info). Two runs at different paths would therefore produce binaries
    // that differ at the byte locations where those strings live.
    //
    // Also remap CARGO_HOME and CARGO_TARGET_DIR for the same reason:
    // registry dependency paths and incremental compilation artifacts
    // can surface in panic strings via inlined helpers from std / proc
    // macros.
    //
    // We append to any host-supplied RUSTFLAGS rather than overwriting:
    // an operator who set RUSTFLAGS for cross-compile linker flags
    // (e.g. `-C linker=<wrapper>`) would silently lose them otherwise.
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let worktree_str = inputs.worktree.to_string_lossy();
    let cargo_home_str = inputs.cargo_home.to_string_lossy();
    let cargo_target_str = inputs.cargo_target.to_string_lossy();
    // RUSTFLAGS is a space-delimited token list with no quoting support.
    // A whitespace-bearing path here would be parsed as multiple args by
    // rustc. Worktree::add already rejects whitespace in the worktree
    // path; we defend cargo_home / cargo_target the same way at this
    // composition site so the constraint is enforced even when the
    // caller bypassed Worktree (e.g. supplying a CARGO_HOME pointing
    // into a system path with embedded spaces).
    for (label, raw) in [
        ("worktree", worktree_str.as_ref()),
        ("cargo_home", cargo_home_str.as_ref()),
        ("cargo_target", cargo_target_str.as_ref()),
    ] {
        if raw.chars().any(char::is_whitespace) {
            panic!(
                "determinism harness {label} path {raw:?} contains whitespace; \
                 RUSTFLAGS has no quoting support and embedded spaces would \
                 misparse --remap-path-prefix. Re-run with a scratch directory \
                 free of whitespace."
            );
        }
    }
    for (from, to) in [
        (worktree_str.as_ref(), "/anodize"),
        (cargo_home_str.as_ref(), "/cargo"),
        (cargo_target_str.as_ref(), "/target"),
    ] {
        if from.is_empty() {
            continue;
        }
        let flag = format!("--remap-path-prefix={}={}", from, to);
        if !rustflags.is_empty() {
            rustflags.push(' ');
        }
        rustflags.push_str(&flag);
    }
    if !rustflags.is_empty() {
        env.insert("RUSTFLAGS".into(), rustflags.clone());
    }

    // Windows MSVC determinism flags. See [target.*] rustflags in
    // `.cargo/config.toml` for the non-harness path. This per-target
    // env var path is required because the harness sets RUSTFLAGS for
    // `--remap-path-prefix`, which (per cargo precedence) suppresses
    // the `[target.<triple>] rustflags` config entry.
    //
    // The flag set:
    //   - `-C codegen-units=1` — single codegen unit so cross-CU
    //     function-ordering non-determinism doesn't shuffle the
    //     resulting object's symbol/section layout.
    //   - `-C link-arg=/Brepro` — substitute PE `TimeDateStamp` with a
    //     content hash.
    //   - `-C link-arg=/OPT:NOICF` — disable Identical COMDAT Folding.
    //   - `-C link-arg=/INCREMENTAL:NO` — disable incremental linking.
    //   - `-C link-arg=/DEBUG:NONE` — do not emit PDB or CodeView
    //     records.
    //   - `-C strip=symbols` — drop the COFF symbol table.
    let msvc_flags = [
        "-C codegen-units=1",
        "-C link-arg=/Brepro",
        "-C link-arg=/OPT:NOICF",
        "-C link-arg=/INCREMENTAL:NO",
        "-C link-arg=/DEBUG:NONE",
        "-C strip=symbols",
    ];
    for triple in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let mut per_target = rustflags.clone();
        for flag in msvc_flags {
            if !per_target.is_empty() {
                per_target.push(' ');
            }
            per_target.push_str(flag);
        }
        let key = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            triple.replace('-', "_").to_uppercase()
        );
        env.insert(key, per_target);
    }

    // On Windows, the host build (e.g. `cargo run --release` invoked
    // by a `before:` hook) lands at `target/release/anodizer.exe`.
    // Cargo's host build reads global `RUSTFLAGS` (not the per-target
    // `CARGO_TARGET_<HOST>_RUSTFLAGS`, which only applies when
    // `--target=<HOST>` is explicit). Safe on Windows runners because
    // the host triple IS msvc, so the link.exe-specific flags are
    // valid for every build (proc-macros, build scripts, etc.).
    if cfg!(windows) {
        for flag in msvc_flags {
            if !rustflags.is_empty() {
                rustflags.push(' ');
            }
            rustflags.push_str(flag);
        }
        env.insert("RUSTFLAGS".into(), rustflags.clone());
    }

    // Inherit only the explicit allow-list of identity-only host env so
    // build scripts that conditionally embed git/CI info still work, and
    // no credential-bearing vars (GITHUB_TOKEN, ACTIONS_RUNTIME_TOKEN,
    // etc.) leak into the child.
    for &key in HARNESS_ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    // Windows env is sprawling; cc-rs / cargo / rustc rely on
    // PROGRAMFILES*, WINDIR, SystemRoot, PROCESSOR_*, USERPROFILE,
    // APPDATA, LOCALAPPDATA, TEMP, TMP, PATHEXT, and the entire MSVC
    // toolchain block. Enumerating each in the allow-list is fragile.
    // Instead: inherit everything from the host env and drop the
    // credential deny-list + suffix sweep + GH/RUNNER hermeticity
    // sweep (see [`windows_env_should_drop`]).
    #[cfg(windows)]
    for (key, value) in std::env::vars() {
        if windows_env_should_drop(&key) {
            continue;
        }
        env.entry(key).or_insert(value);
    }
    // rustup needs RUSTUP_HOME to dispatch a toolchain; on GH Actions
    // runners (and most dev machines) it isn't set in the env — rustup
    // defaults to $HOME/.rustup. Since the child runs with HOME=tmpdir,
    // we must compute the default from the HOST's HOME (Unix) or
    // USERPROFILE (Windows) and propagate it explicitly.
    env.entry("RUSTUP_HOME".into()).or_insert_with(|| {
        let host_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        host_home.join(".rustup").to_string_lossy().into_owned()
    });
    // Always set CI=true so build scripts know they're in a sealed env.
    env.entry("CI".into()).or_insert_with(|| "true".into());

    // Inserted last so the harness's ephemeral material wins over any
    // host-leaked credential vars under either the allow-list or the
    // Windows inherit pass.
    if let Some(keys) = inputs.signing_keys {
        env.insert("ANODIZER_IN_DETERMINISM_HARNESS".into(), "1".into());
        env.insert("COSIGN_KEY".into(), keys.cosign_key_contents.clone());
        env.insert("COSIGN_PASSWORD".into(), keys.cosign_password.clone());
        env.insert(
            "GNUPGHOME".into(),
            anodizer_core::harness_signing::path_for_subprocess_env(&keys.gnupg_home),
        );
        env.insert("GPG_FINGERPRINT".into(), keys.gpg_fingerprint.clone());
        env.insert("GPG_TTY".into(), "/dev/null".into());
        env.insert(
            "GPG_KEY_PATH".into(),
            anodizer_core::harness_signing::path_for_subprocess_env(&keys.gpg_key_path),
        );
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn inputs<'a>(scratch: &'a Path) -> BuildSubprocessEnv<'a> {
        BuildSubprocessEnv {
            cargo_home: scratch,
            cargo_target: scratch,
            tmpdir: scratch,
            home_dir: scratch,
            sde: 1_715_000_000,
            worktree: scratch,
            signing_keys: None,
        }
    }

    fn with_cleared<F: FnOnce()>(keys: &[&str], f: F) {
        // SAFETY: gated by `#[serial(harness_env)]` on every caller.
        for k in keys {
            unsafe { std::env::remove_var(k) };
        }
        f();
        for k in keys {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn allow_listed_path_inherits_host_path() {
        let expected = std::env::var("PATH").unwrap_or_default();
        assert_eq!(allow_listed_path(), expected);
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_does_not_leak_github_token() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["GITHUB_TOKEN"], || {
            unsafe { std::env::set_var("GITHUB_TOKEN", "ghp_secret_value") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("GITHUB_TOKEN"),
                "GITHUB_TOKEN must NOT propagate into the harness subprocess env"
            );
            assert!(
                !env.values().any(|v| v == "ghp_secret_value"),
                "no env entry may carry the token value"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_does_not_leak_actions_runtime_token() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["ACTIONS_RUNTIME_TOKEN"], || {
            unsafe { std::env::set_var("ACTIONS_RUNTIME_TOKEN", "actions_secret") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
                "ACTIONS_RUNTIME_TOKEN must NOT propagate into the harness subprocess env"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_does_not_leak_actions_cache_url() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["ACTIONS_CACHE_URL"], || {
            unsafe { std::env::set_var("ACTIONS_CACHE_URL", "https://cache.example") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("ACTIONS_CACHE_URL"),
                "ACTIONS_CACHE_URL must NOT propagate (network-reach surface)"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_includes_github_repository_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["GITHUB_REPOSITORY"], || {
            unsafe { std::env::set_var("GITHUB_REPOSITORY", "toss45/anodizer") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("GITHUB_REPOSITORY").map(String::as_str),
                Some("toss45/anodizer"),
                "GITHUB_REPOSITORY is identity and must propagate"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_includes_github_sha_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["GITHUB_SHA"], || {
            unsafe { std::env::set_var("GITHUB_SHA", "deadbeefcafe") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("GITHUB_SHA").map(String::as_str),
                Some("deadbeefcafe"),
                "GITHUB_SHA is identity and must propagate"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_includes_runner_identity_vars_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            ("RUNNER_OS", "Linux"),
            ("RUNNER_ARCH", "X64"),
            ("RUNNER_NAME", "self-hosted-1"),
        ];
        with_cleared(&cases.map(|(k, _)| k), || {
            for (k, v) in cases {
                unsafe { std::env::set_var(k, v) };
            }
            let env = build_subprocess_env(&inputs(tmp.path()));
            for (k, v) in cases {
                assert_eq!(
                    env.get(k).map(String::as_str),
                    Some(v),
                    "{k} is identity and must propagate (value `{v}`)"
                );
            }
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_omits_unset_github_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let all_identity = [
            "GITHUB_REPOSITORY",
            "GITHUB_SHA",
            "GITHUB_REF",
            "GITHUB_REF_NAME",
            "GITHUB_RUN_ID",
            "GITHUB_RUN_NUMBER",
            "GITHUB_WORKFLOW",
            "GITHUB_ACTOR",
        ];
        with_cleared(&all_identity, || {
            let env = build_subprocess_env(&inputs(tmp.path()));
            for k in all_identity {
                assert!(
                    !env.contains_key(k),
                    "unset host var `{k}` must not appear in env (no empty-string default)"
                );
            }
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_does_not_leak_runner_temp() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUNNER_TEMP"], || {
            unsafe { std::env::set_var("RUNNER_TEMP", "/some/host/tmpdir") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("RUNNER_TEMP"),
                "RUNNER_TEMP must NOT propagate — harness owns TMPDIR"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_sets_ci_true_when_host_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["CI"], || {
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("CI").map(String::as_str),
                Some("true"),
                "harness defaults CI=true when host has no CI var set"
            );
        });
    }

    /// Restore the host's HOME on Drop so RUSTUP_HOME tests can mutate
    /// it under the serial(harness_env) lock without leaking a fake
    /// value into sibling tests.
    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
    }
    impl HomeGuard {
        fn capture() -> Self {
            Self {
                previous: std::env::var_os("HOME"),
            }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_defaults_rustup_home_from_host_home_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::capture();
        with_cleared(&["RUSTUP_HOME"], || {
            unsafe { std::env::set_var("HOME", "/host/home/user") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            let rh = env
                .get("RUSTUP_HOME")
                .expect("RUSTUP_HOME must be defaulted when unset")
                .replace('\\', "/");
            assert_eq!(
                rh, "/host/home/user/.rustup",
                "harness must default RUSTUP_HOME to <host HOME>/.rustup"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_rustup_home_explicit_wins_over_default() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::capture();
        with_cleared(&["RUSTUP_HOME"], || {
            unsafe { std::env::set_var("HOME", "/host/home/user") };
            unsafe { std::env::set_var("RUSTUP_HOME", "/operator/override") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("RUSTUP_HOME").map(String::as_str),
                Some("/operator/override"),
                "an explicit host RUSTUP_HOME must take precedence over the synthesized default"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_inherits_host_system_vars() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["PROGRAMFILES"], || {
            unsafe { std::env::set_var("PROGRAMFILES", r"C:\fake\Program Files") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("PROGRAMFILES").map(String::as_str),
                Some(r"C:\fake\Program Files"),
                "Windows pass must inherit non-credential host system vars (PROGRAMFILES is load-bearing for cc-rs link.exe discovery)"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_drops_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = [
            "GITHUB_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "SOMETHING_TOKEN",
            "SOMETHING_PASSWORD",
        ];
        with_cleared(&keys, || {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", "ghp_x");
                std::env::set_var("CARGO_REGISTRY_TOKEN", "cratesio_y");
                std::env::set_var("SOMETHING_TOKEN", "z");
                std::env::set_var("SOMETHING_PASSWORD", "w");
            }
            let env = build_subprocess_env(&inputs(tmp.path()));
            for k in keys {
                assert!(
                    !env.contains_key(k),
                    "credential-bearing host var `{k}` must NOT propagate on Windows"
                );
            }
            for v in ["ghp_x", "cratesio_y", "z", "w"] {
                assert!(
                    !env.values().any(|got| got == v),
                    "credential value `{v}` leaked under a different key"
                );
            }
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_drops_actions_workflow_internals() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["ACTIONS_RUNTIME_TOKEN"], || {
            unsafe { std::env::set_var("ACTIONS_RUNTIME_TOKEN", "actions_x") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
                "ACTIONS_* workflow-internal vars must be dropped by the Windows pass"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_drops_runner_temp_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUNNER_TEMP"], || {
            unsafe { std::env::set_var("RUNNER_TEMP", r"C:\fake\temp") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("RUNNER_TEMP"),
                "RUNNER_TEMP must NOT propagate on Windows — it points at the runner's on-host scratch and the harness owns TMPDIR"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_drops_runner_workspace_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUNNER_WORKSPACE"], || {
            unsafe { std::env::set_var("RUNNER_WORKSPACE", r"C:\fake\workspace") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("RUNNER_WORKSPACE"),
                "RUNNER_WORKSPACE must NOT propagate on Windows — host workflow state, not identity"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_drops_github_workspace_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["GITHUB_WORKSPACE"], || {
            unsafe { std::env::set_var("GITHUB_WORKSPACE", r"C:\fake\gh_workspace") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert!(
                !env.contains_key("GITHUB_WORKSPACE"),
                "GITHUB_WORKSPACE must NOT propagate on Windows — points at the GH-runner-owned checkout, not the hermetic worktree"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_injects_remap_path_prefix_for_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUSTFLAGS"], || {
            let env = build_subprocess_env(&inputs(tmp.path()));
            let rf = env
                .get("RUSTFLAGS")
                .expect("RUSTFLAGS must be injected so worktree paths don't leak into the binary");
            let needle = format!(
                "--remap-path-prefix={}=/anodize",
                tmp.path().to_string_lossy()
            );
            assert!(
                rf.contains(&needle),
                "RUSTFLAGS must remap the worktree path. got={rf}, expected substring={needle}"
            );
            assert!(
                rf.contains("=/cargo"),
                "CARGO_HOME must be remapped to /cargo"
            );
            assert!(
                rf.contains("=/target"),
                "CARGO_TARGET_DIR must be remapped to /target"
            );
        });
    }

    #[test]
    #[serial(harness_env)]
    fn harness_env_preserves_host_rustflags() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUSTFLAGS"], || {
            unsafe { std::env::set_var("RUSTFLAGS", "-C linker=link.exe -C link-arg=/DEBUG") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            let rf = env.get("RUSTFLAGS").unwrap();
            assert!(
                rf.contains("-C linker=link.exe"),
                "host RUSTFLAGS must survive the harness append. got={rf}"
            );
            assert!(
                rf.contains("--remap-path-prefix="),
                "remap-path-prefix must be appended even when host RUSTFLAGS is set. got={rf}"
            );
        });
    }

    /// Regression: the harness MUST inject
    /// `CARGO_TARGET_<msvc-triple>_RUSTFLAGS=-C link-arg=/Brepro` so two
    /// harness runs produce byte-identical `anodizer.exe` binaries.
    /// Without `/Brepro`, link.exe stamps the PE COFF `TimeDateStamp`
    /// with wall-clock time and the .exe (plus every archive wrapping
    /// it) drifts.
    ///
    /// Per-target (not global) because `/Brepro` is link.exe-only;
    /// lld/ld would reject the flag.
    ///
    /// Per-target RUSTFLAGS must ALSO carry the remap-path-prefix
    /// entries: cargo precedence is `CARGO_TARGET_<triple>_RUSTFLAGS`
    /// > `RUSTFLAGS`, so the per-target value REPLACES (not merges
    /// with) the global.
    #[test]
    #[serial(harness_env)]
    fn harness_env_injects_msvc_determinism_flags() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUSTFLAGS"], || {
            let env = build_subprocess_env(&inputs(tmp.path()));
            for triple_env in [
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS",
            ] {
                let rf = env.get(triple_env).unwrap_or_else(|| {
                    panic!("{triple_env} must be injected so link.exe gets /Brepro")
                });
                for needle in ["-C link-arg=/Brepro", "-C link-arg=/DEBUG:NONE"] {
                    assert!(
                        rf.contains(needle),
                        "{triple_env} must carry `{needle}`. got={rf}"
                    );
                }
                assert!(
                    rf.contains("--remap-path-prefix="),
                    "{triple_env} must also carry --remap-path-prefix. got={rf}"
                );
            }
            // Linux / macOS targets must NOT get a per-target
            // entry — `/Brepro` would error on lld/ld.
            for triple_env in [
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
                "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS",
            ] {
                assert!(
                    !env.contains_key(triple_env),
                    "{triple_env} must NOT be injected — /Brepro is link.exe-only"
                );
            }
        });
    }

    /// Regression: on Windows, global RUSTFLAGS must ALSO carry the
    /// MSVC determinism flags so the host build (e.g.
    /// `cargo run --release` invoked by a `before:` hook, which has
    /// no `--target` and therefore reads global RUSTFLAGS) lands a
    /// byte-stable `target/release/anodizer.exe`.
    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_injects_msvc_flags_into_global_rustflags() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUSTFLAGS"], || {
            let env = build_subprocess_env(&inputs(tmp.path()));
            let rf = env.get("RUSTFLAGS").expect(
                "RUSTFLAGS must be set on Windows so host builds (no --target) are reproducible",
            );
            for needle in ["-C link-arg=/Brepro", "-C link-arg=/DEBUG:NONE"] {
                assert!(
                    rf.contains(needle),
                    "global RUSTFLAGS must carry `{needle}` on Windows. got={rf}"
                );
            }
            assert!(
                rf.contains("--remap-path-prefix="),
                "global RUSTFLAGS must also carry --remap-path-prefix. got={rf}"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    #[serial(harness_env)]
    fn harness_env_windows_keeps_runner_os_allow_listed() {
        let tmp = tempfile::tempdir().unwrap();
        with_cleared(&["RUNNER_OS"], || {
            unsafe { std::env::set_var("RUNNER_OS", "Windows") };
            let env = build_subprocess_env(&inputs(tmp.path()));
            assert_eq!(
                env.get("RUNNER_OS").map(String::as_str),
                Some("Windows"),
                "RUNNER_OS is on the identity allow-list and MUST propagate even though the namespace gate would otherwise drop it"
            );
        });
    }
}
