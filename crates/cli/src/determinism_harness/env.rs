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

use anodizer_core::env_source::{EnvSource, ProcessEnvSource};
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
    // Operator color preferences. The child's stderr is inherited into
    // the harness's own stream, so the parent's color contract must
    // extend to the child — without these the sealed CI=true would
    // force color in the child even when the operator set NO_COLOR.
    "NO_COLOR",
    "ANODIZER_COLOR",
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
/// Check order: explicit deny-list → ACTIONS_* / RUNNER_TOKEN →
/// rustflags-family sweep (`/Brepro`-precedence guard) → credential suffix
/// sweep → GH/RUNNER namespace hermeticity gate.
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
    // Drop every inherited rustflags-family var. The harness builds its own
    // authoritative RUSTFLAGS / CARGO_TARGET_<msvc>_RUSTFLAGS carrying
    // `/Brepro` (the flag that makes the PE COFF TimeDateStamp a content
    // hash instead of wall-clock). Cargo picks ONE rustflags source,
    // first-present-wins with NO merge, and CARGO_ENCODED_RUSTFLAGS sits at
    // the top of that order — so a host-supplied one (Cargo exports it into
    // the `cargo run`-launched anodizer process) would silently out-precedence
    // the harness's `/Brepro` injection, yielding a non-reproducible binary.
    if key.eq_ignore_ascii_case("CARGO_ENCODED_RUSTFLAGS")
        || key.eq_ignore_ascii_case("RUSTFLAGS")
        || key.eq_ignore_ascii_case("CARGO_BUILD_RUSTFLAGS")
    {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    if lower.starts_with("cargo_target_") && lower.ends_with("_rustflags") {
        return true;
    }
    // Compile-cache wrapper + its config namespace. A determinism probe must
    // build from clean: a cache can serve a non-reproducible cached object
    // (spurious .text drift) and cold-start-fails when its backend creds are
    // stripped by this hermetic env. The empty-string inserts above are the
    // authoritative override; this keeps the passthrough from re-adding them.
    if key.eq_ignore_ascii_case("RUSTC_WRAPPER")
        || key.eq_ignore_ascii_case("RUSTC_WORKSPACE_WRAPPER")
        || lower.starts_with("sccache_")
    {
        return true;
    }
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
    /// `GPG_KEY_PATH` into the child env.
    /// (`ANODIZER_IN_DETERMINISM_HARNESS=1` is exported for every child,
    /// keys or not.)
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
///
/// `env` is the injectable host-env source — production routes through
/// [`ProcessEnvSource`]; tests inject a closed `MapEnvSource` so they
/// drive the read without process-env mutation.
pub(super) fn allow_listed_path_with_env(env: &dyn EnvSource) -> String {
    env.var("PATH").unwrap_or_default()
}

/// Process-env convenience wrapper over [`allow_listed_path_with_env`].
///
/// Kept for symmetry with [`build_subprocess_env`] — the harness
/// orchestrator drives PATH through [`build_subprocess_env_with_env`]'s
/// allow-list re-population, never via this helper directly.
#[allow(dead_code)]
pub(super) fn allow_listed_path() -> String {
    allow_listed_path_with_env(&ProcessEnvSource)
}

/// Pure constructor for the child env map, reading host-env values
/// through the injected [`EnvSource`].
///
/// Production wires this with [`ProcessEnvSource`] (which delegates to
/// `std::env::var` / `std::env::vars`) and a runtime
/// [`anodizer_core::determinism::host_is_windows_msvc`] probe via
/// [`build_subprocess_env`]. Tests pass a closed `MapEnvSource` and an
/// explicit `host_is_windows_msvc` to drive every branch — credential leak,
/// identity propagation, Windows namespace gate, the global-RUSTFLAGS MSVC
/// injection — on any host, without mutating process env.
///
/// `host_is_windows_msvc` is the injected host-OS decision: when `true`, the
/// global RUSTFLAGS gains the [`MSVC_DETERMINISM_RUSTFLAGS`] set so the
/// host (`--target`-less) build is reproducible. It is a parameter — not a
/// `cfg!(windows)` check — so a cross-built harness binary keys off the OS
/// it RUNS on, not the one it was BUILT on.
///
/// [`MSVC_DETERMINISM_RUSTFLAGS`]: anodizer_core::determinism::MSVC_DETERMINISM_RUSTFLAGS
pub(crate) fn build_subprocess_env_with_env(
    inputs: &BuildSubprocessEnv<'_>,
    host_env: &dyn EnvSource,
    host_is_windows_msvc: bool,
) -> HashMap<String, String> {
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

    // Restore docker-buildx builder reachability for the `docker` stage's OCI
    // exporter. The HOME override above hermetically seals the child away from
    // the runner's real `~/.docker`, where the workflow-provisioned
    // `docker-container` builder is recorded (`<DOCKER_CONFIG>/buildx/current`).
    // Without it the child `docker buildx build` falls back to the default
    // `docker` driver, which cannot serve `--output=type=oci`, and the docker
    // stage aborts. Point DOCKER_CONFIG at the runner's real docker config dir
    // so the current builder is found despite the sealed HOME:
    //   - an explicit host DOCKER_CONFIG wins (operator override / non-default
    //     config dir),
    //   - otherwise derive `<real HOME>/.docker` from the host's pre-override
    //     HOME (the GH Actions default location).
    // Reproducibility-neutral: OCI output bytes are fixed by the Dockerfile +
    // build context + SOURCE_DATE_EPOCH / --rewrite-timestamp, never by which
    // builder runs or where its config lives. Safe to set whenever derivable —
    // if no builder is provisioned there, buildx behaves exactly as before
    // (default-driver fallback), so there is no regression on hosts without a
    // `docker-container` builder.
    if let Some(docker_config) = host_env.var("DOCKER_CONFIG").or_else(|| {
        host_env.var("HOME").map(|home| {
            Path::new(&home)
                .join(".docker")
                .to_string_lossy()
                .into_owned()
        })
    }) {
        env.insert("DOCKER_CONFIG".into(), docker_config);
    }

    env.insert("SOURCE_DATE_EPOCH".into(), inputs.sde.to_string());
    env.insert("PATH".into(), allow_listed_path_with_env(host_env));

    // A determinism probe must build from clean with no compile-cache
    // wrapper: a cache can serve a non-reproducible cached object across
    // rebuilds (spurious .text drift) and, when its backend creds are
    // stripped by this hermetic env, fail to cold-start the cache server
    // and abort the build. Empty value = cargo treats the wrapper as unset.
    // Set before BOTH the allow-list and the Windows passthrough loops so
    // neither can reintroduce an inherited host value.
    env.insert("RUSTC_WRAPPER".into(), String::new());
    env.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());

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
    let mut rustflags = host_env.var("RUSTFLAGS").unwrap_or_default();
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

    // Windows MSVC determinism flags. The canonical flag set lives in
    // `anodizer_core::determinism::MSVC_DETERMINISM_RUSTFLAGS` (also
    // mirrored in `[target.*] rustflags` in `.cargo/config.toml`). This
    // per-target env var path is required because the harness sets RUSTFLAGS
    // for `--remap-path-prefix`, which (per cargo precedence) suppresses the
    // `[target.<triple>] rustflags` config entry. `merge_*` deduplicates so
    // an inherited token (host RUSTFLAGS carrying `/Brepro`) is not doubled.
    for triple in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let per_target = anodizer_core::determinism::merge_msvc_determinism_rustflags(&rustflags);
        let key = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            triple.replace('-', "_").to_uppercase()
        );
        env.insert(key, per_target);
    }

    // When the host running this harness is itself windows-msvc, the host
    // build (e.g. `cargo run --release` invoked by a `before:` hook) lands at
    // `target/release/anodizer.exe`. Cargo's host build reads global
    // `RUSTFLAGS` (not the per-target `CARGO_TARGET_<HOST>_RUSTFLAGS`, which
    // only applies when `--target=<HOST>` is explicit), so the global set
    // must also carry `/Brepro` or the host .exe drifts at PE offset 0x108.
    //
    // RUNTIME host detection, not `cfg!(windows)`: the harness binary may be
    // built on a different OS than it runs on (and consumers run
    // `anodizer check determinism` locally on Windows from binaries built
    // anywhere). A compile-time check reads the BUILD os, not the RUN os, and
    // would silently skip the injection on a cross-built binary — the exact
    // failure class this guards. The MSVC link.exe flags are only valid on a
    // windows-msvc host, so they must NOT be added on any other host.
    if host_is_windows_msvc {
        rustflags = anodizer_core::determinism::merge_msvc_determinism_rustflags(&rustflags);
        env.insert("RUSTFLAGS".into(), rustflags.clone());
    }

    // Inherit only the explicit allow-list of identity-only host env so
    // build scripts that conditionally embed git/CI info still work, and
    // no credential-bearing vars (GITHUB_TOKEN, ACTIONS_RUNTIME_TOKEN,
    // etc.) leak into the child.
    for &key in HARNESS_ENV_ALLOWLIST {
        if let Some(v) = host_env.var(key) {
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
    for (key, value) in host_env.vars() {
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
        let host_home = host_env
            .var("HOME")
            .or_else(|| host_env.var("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        host_home.join(".rustup").to_string_lossy().into_owned()
    });
    // Always set CI=true so build scripts know they're in a sealed env.
    env.entry("CI".into()).or_insert_with(|| "true".into());

    // Replica children replay the SAME static config the operator's outer
    // invocation just loaded (which already emitted the config-derived
    // warnings — legacy aliases, submitter `required: true`, etc.); letting
    // every child re-emit them duplicates each warning runs× in one console.
    // Those warnings travel via `tracing::warn!`, so cap the child's tracing
    // filter at `error`. StageLogger output (stage progress, runtime
    // warnings) is unaffected — it doesn't route through tracing.
    // `.entry().or_insert()` keeps an operator-supplied RUST_LOG (Windows
    // inherit-everything pass) authoritative for debugging.
    env.entry("RUST_LOG".into())
        .or_insert_with(|| "error".into());

    // Marker for "this release run is a determinism-harness replica".
    // Surfaced to user templates as `IsHarness` (see
    // `Context::populate_runtime_vars`) so `if_condition:` blocks can opt
    // stages in/out inside the harness. Set unconditionally — the property
    // holds for every replica child, not only the signing-key runs that
    // historically carried it. Identical across runs, so byte-comparison
    // is unaffected.
    env.insert("ANODIZER_IN_DETERMINISM_HARNESS".into(), "1".into());

    // Redirect LLVM coverage profile output OUTSIDE the worktree so an
    // instrumented anodize child (built via `cargo llvm-cov`) doesn't
    // drop `default_*.profraw` files into the source tree on process
    // exit. The LLVM coverage runtime defaults to a relative path
    // resolved against the process's CWD, which the harness sets to the
    // worktree root — those .profraw files then get swept up by
    // `cargo package --allow-dirty` (D1 stage) and embedded in the
    // `.crate` tarball. Different PIDs across the two harness runs
    // produce different filenames, so the .crate hashes drift and the
    // byte-stability test fails.
    //
    // The path must be outside the worktree because cargo walks
    // `.det-tmp/` subdirectories that contain files (cargo's
    // auto-exclude is `target/`-shaped, not "anything under
    // `.det-tmp/`"). Falling back to the system temp dir is the simplest
    // host-OS-portable path that avoids polluting the source tree. The
    // `%m` (module signature) + `%p` (PID) substitutions remain so
    // concurrent coverage from multiple subprocesses doesn't collide.
    // Unconditional — non-instrumented binaries ignore the var.
    let llvm_profraw = std::env::temp_dir()
        .join("anodize-harness-llvm")
        .join("default_%m_%p.profraw");
    env.insert(
        "LLVM_PROFILE_FILE".into(),
        llvm_profraw.to_string_lossy().into_owned(),
    );

    // Inserted last so the harness's ephemeral material wins over any
    // host-leaked credential vars under either the allow-list or the
    // Windows inherit pass.
    if let Some(keys) = inputs.signing_keys {
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

/// Process-env convenience wrapper over [`build_subprocess_env_with_env`].
///
/// The harness drives this from a single-threaded orchestrator, so the
/// `std::env::vars()` snapshot inside [`ProcessEnvSource`] is consistent
/// for the lifetime of the call. The host-OS decision is resolved at
/// RUNTIME via [`anodizer_core::determinism::host_is_windows_msvc`]
/// (a `rustc -vV` probe) so it reflects the machine the harness is running
/// on, even for a cross-built binary.
pub(crate) fn build_subprocess_env(inputs: &BuildSubprocessEnv<'_>) -> HashMap<String, String> {
    build_subprocess_env_with_env(
        inputs,
        &ProcessEnvSource,
        anodizer_core::determinism::host_is_windows_msvc(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::env_source::MapEnvSource;

    fn inputs(scratch: &Path) -> BuildSubprocessEnv<'_> {
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

    /// Drive `build_subprocess_env_with_env` against a closed `MapEnvSource`
    /// seeded from the supplied `(key, value)` fixtures, with a NON-windows
    /// host (the common case for the env-policy assertions below).
    fn build_with(scratch: &Path, host: &[(&str, &str)]) -> HashMap<String, String> {
        build_with_host(scratch, host, false)
    }

    /// `build_with` with an explicit host-windows-msvc decision so the
    /// global-RUSTFLAGS MSVC injection can be exercised on any host (the
    /// injectable seam that lets the Windows regression run on Linux CI).
    fn build_with_host(
        scratch: &Path,
        host: &[(&str, &str)],
        host_is_windows_msvc: bool,
    ) -> HashMap<String, String> {
        let mut map = MapEnvSource::new();
        for (k, v) in host {
            map.set(*k, *v);
        }
        build_subprocess_env_with_env(&inputs(scratch), &map, host_is_windows_msvc)
    }

    #[test]
    fn allow_listed_path_reads_through_env_source() {
        let env = MapEnvSource::new().with("PATH", "/fixture/bin:/usr/bin");
        assert_eq!(allow_listed_path_with_env(&env), "/fixture/bin:/usr/bin");
    }

    /// Static-config warnings (legacy aliases, submitter `required: true`)
    /// must print once per harness invocation — from the outer process —
    /// not once per replica child. The children's tracing filter is capped
    /// at `error` to enforce that.
    #[test]
    fn harness_env_caps_child_tracing_at_error() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
        assert_eq!(
            env.get("RUST_LOG").map(String::as_str),
            Some("error"),
            "child env must carry RUST_LOG=error so replica builds don't \
             re-emit the outer process's static-config warnings"
        );
    }

    /// `IsHarness` (template var) keys off this marker; it must be set for
    /// EVERY replica child, not only signing-key runs.
    #[test]
    fn harness_env_always_marks_determinism_children() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
        assert_eq!(
            env.get("ANODIZER_IN_DETERMINISM_HARNESS")
                .map(String::as_str),
            Some("1"),
            "harness marker must be present without signing keys"
        );
    }

    #[test]
    fn harness_env_does_not_leak_github_token() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("GITHUB_TOKEN", "ghp_secret_value")]);
        assert!(
            !env.contains_key("GITHUB_TOKEN"),
            "GITHUB_TOKEN must NOT propagate into the harness subprocess env"
        );
        assert!(
            !env.values().any(|v| v == "ghp_secret_value"),
            "no env entry may carry the token value"
        );
    }

    #[test]
    fn harness_env_does_not_leak_actions_runtime_token() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("ACTIONS_RUNTIME_TOKEN", "actions_secret")]);
        assert!(
            !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
            "ACTIONS_RUNTIME_TOKEN must NOT propagate into the harness subprocess env"
        );
    }

    #[test]
    fn harness_env_does_not_leak_actions_cache_url() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[("ACTIONS_CACHE_URL", "https://cache.example")],
        );
        assert!(
            !env.contains_key("ACTIONS_CACHE_URL"),
            "ACTIONS_CACHE_URL must NOT propagate (network-reach surface)"
        );
    }

    #[test]
    fn harness_env_includes_github_repository_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("GITHUB_REPOSITORY", "toss45/anodizer")]);
        assert_eq!(
            env.get("GITHUB_REPOSITORY").map(String::as_str),
            Some("toss45/anodizer"),
            "GITHUB_REPOSITORY is identity and must propagate"
        );
    }

    #[test]
    fn harness_env_includes_github_sha_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("GITHUB_SHA", "deadbeefcafe")]);
        assert_eq!(
            env.get("GITHUB_SHA").map(String::as_str),
            Some("deadbeefcafe"),
            "GITHUB_SHA is identity and must propagate"
        );
    }

    #[test]
    fn harness_env_includes_runner_identity_vars_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            ("RUNNER_OS", "Linux"),
            ("RUNNER_ARCH", "X64"),
            ("RUNNER_NAME", "self-hosted-1"),
        ];
        let env = build_with(tmp.path(), &cases);
        for (k, v) in cases {
            assert_eq!(
                env.get(k).map(String::as_str),
                Some(v),
                "{k} is identity and must propagate (value `{v}`)"
            );
        }
    }

    /// With no explicit host `DOCKER_CONFIG`, the harness derives it from the
    /// host's pre-override HOME so the HOME-sealed child still finds the
    /// runner-provisioned `docker-container` builder for the OCI exporter.
    #[test]
    fn harness_env_derives_docker_config_from_host_home() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("HOME", "/home/runner")]);
        // Mirror the production `Path::join` so the expectation is
        // separator-portable: `/home/runner/.docker` on Unix,
        // `/home/runner\.docker` on the Windows test shard.
        let expected = std::path::Path::new("/home/runner")
            .join(".docker")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            env.get("DOCKER_CONFIG").map(String::as_str),
            Some(expected.as_str()),
            "DOCKER_CONFIG must derive from the real host HOME, not the sealed child HOME"
        );
    }

    /// An explicit host `DOCKER_CONFIG` (operator override / non-default config
    /// dir) wins over the HOME-derived default.
    #[test]
    fn harness_env_passes_through_explicit_docker_config() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[
                ("HOME", "/home/runner"),
                ("DOCKER_CONFIG", "/custom/docker"),
            ],
        );
        assert_eq!(
            env.get("DOCKER_CONFIG").map(String::as_str),
            Some("/custom/docker"),
            "explicit host DOCKER_CONFIG must pass through, overriding the HOME-derived default"
        );
    }

    /// With neither host HOME nor host `DOCKER_CONFIG` there is nothing to
    /// derive from, so the child carries no `DOCKER_CONFIG` (no empty-string
    /// default that would mis-point buildx).
    #[test]
    fn harness_env_omits_docker_config_when_underivable() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
        assert!(
            !env.contains_key("DOCKER_CONFIG"),
            "DOCKER_CONFIG must be absent when neither HOME nor DOCKER_CONFIG is set on the host"
        );
    }

    #[test]
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
        let env = build_with(tmp.path(), &[]);
        for k in all_identity {
            assert!(
                !env.contains_key(k),
                "unset host var `{k}` must not appear in env (no empty-string default)"
            );
        }
    }

    #[test]
    fn harness_env_does_not_leak_runner_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("RUNNER_TEMP", "/some/host/tmpdir")]);
        assert!(
            !env.contains_key("RUNNER_TEMP"),
            "RUNNER_TEMP must NOT propagate — harness owns TMPDIR"
        );
    }

    /// A determinism probe must build with NO compile-cache wrapper. The
    /// Windows passthrough historically leaked `RUSTC_WRAPPER=sccache` into
    /// the hermetic child, which then cold-start-failed (creds stripped) and
    /// aborted the build. Both wrapper vars must be neutralized to the empty
    /// string (cargo treats an empty wrapper as unset) regardless of what the
    /// host env supplies.
    #[test]
    fn harness_env_neutralizes_compile_cache_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[
                ("RUSTC_WRAPPER", "sccache"),
                ("RUSTC_WORKSPACE_WRAPPER", "sccache"),
                ("SCCACHE_GHA_ENABLED", "true"),
            ],
        );
        assert_eq!(
            env.get("RUSTC_WRAPPER").map(String::as_str),
            Some(""),
            "RUSTC_WRAPPER must be neutralized to empty so cargo runs no compile cache"
        );
        assert_eq!(
            env.get("RUSTC_WORKSPACE_WRAPPER").map(String::as_str),
            Some(""),
            "RUSTC_WORKSPACE_WRAPPER must be neutralized to empty"
        );
    }

    /// Regression: the Windows inherit-everything pass must not let a host
    /// `RUSTC_WRAPPER=sccache` survive. The passthrough loop's
    /// `env.entry(key).or_insert(value)` could only clobber the authoritative
    /// empty if the wrapper keys weren't dropped first — the original
    /// failure was a Windows-only non-reproducible build served from sccache.
    #[test]
    #[cfg(windows)]
    fn harness_env_windows_neutralizes_compile_cache_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[
                ("RUSTC_WRAPPER", "sccache"),
                ("RUSTC_WORKSPACE_WRAPPER", "sccache"),
                ("SCCACHE_GHA_ENABLED", "true"),
                // A normal host var that SHOULD pass through, proving the
                // inherit-everything loop actually ran.
                ("WINDIR", r"C:\Windows"),
            ],
        );
        assert_eq!(
            env.get("RUSTC_WRAPPER").map(String::as_str),
            Some(""),
            "host RUSTC_WRAPPER=sccache must NOT clobber the authoritative empty on Windows"
        );
        assert_eq!(
            env.get("RUSTC_WORKSPACE_WRAPPER").map(String::as_str),
            Some(""),
            "host RUSTC_WORKSPACE_WRAPPER=sccache must NOT clobber the authoritative empty on Windows"
        );
        assert!(
            !env.contains_key("SCCACHE_GHA_ENABLED"),
            "sccache_* config namespace must be dropped by the Windows pass"
        );
        assert_eq!(
            env.get("WINDIR").map(String::as_str),
            Some(r"C:\Windows"),
            "non-credential host vars must still inherit, proving the passthrough loop ran"
        );
    }

    /// Direct coverage of the predicate driving the Windows wrapper drop:
    /// case-insensitive name match plus the `sccache_` config-namespace
    /// prefix, with a negative to prove a normal system var is untouched.
    #[test]
    #[cfg(windows)]
    fn windows_env_should_drop_compile_cache_wrapper() {
        for key in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "rustc_wrapper",
            "SCCACHE_GHA_ENABLED",
            "sccache_dir",
        ] {
            assert!(
                windows_env_should_drop(key),
                "{key} is a compile-cache wrapper/config var and MUST be dropped"
            );
        }
        assert!(
            !windows_env_should_drop("WINDIR"),
            "WINDIR is a load-bearing system var and MUST NOT be dropped"
        );
    }

    #[test]
    fn harness_env_sets_ci_true_when_host_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
        assert_eq!(
            env.get("CI").map(String::as_str),
            Some("true"),
            "harness defaults CI=true when host has no CI var set"
        );
    }

    #[test]
    fn harness_env_defaults_rustup_home_from_host_home_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("HOME", "/host/home/user")]);
        let rh = env
            .get("RUSTUP_HOME")
            .expect("RUSTUP_HOME must be defaulted when unset")
            .replace('\\', "/");
        assert_eq!(
            rh, "/host/home/user/.rustup",
            "harness must default RUSTUP_HOME to <host HOME>/.rustup"
        );
    }

    #[test]
    fn harness_env_rustup_home_explicit_wins_over_default() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[
                ("HOME", "/host/home/user"),
                ("RUSTUP_HOME", "/operator/override"),
            ],
        );
        assert_eq!(
            env.get("RUSTUP_HOME").map(String::as_str),
            Some("/operator/override"),
            "an explicit host RUSTUP_HOME must take precedence over the synthesized default"
        );
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_inherits_host_system_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("PROGRAMFILES", r"C:\fake\Program Files")]);
        assert_eq!(
            env.get("PROGRAMFILES").map(String::as_str),
            Some(r"C:\fake\Program Files"),
            "Windows pass must inherit non-credential host system vars (PROGRAMFILES is load-bearing for cc-rs link.exe discovery)"
        );
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_drops_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = [
            ("GITHUB_TOKEN", "ghp_x"),
            ("CARGO_REGISTRY_TOKEN", "cratesio_y"),
            ("SOMETHING_TOKEN", "z"),
            ("SOMETHING_PASSWORD", "w"),
        ];
        let env = build_with(tmp.path(), &keys);
        for (k, _) in keys {
            assert!(
                !env.contains_key(k),
                "credential-bearing host var `{k}` must NOT propagate on Windows"
            );
        }
        for (_, v) in keys {
            assert!(
                !env.values().any(|got| got == v),
                "credential value `{v}` leaked under a different key"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_drops_actions_workflow_internals() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("ACTIONS_RUNTIME_TOKEN", "actions_x")]);
        assert!(
            !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
            "ACTIONS_* workflow-internal vars must be dropped by the Windows pass"
        );
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_drops_runner_temp_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("RUNNER_TEMP", r"C:\fake\temp")]);
        assert!(
            !env.contains_key("RUNNER_TEMP"),
            "RUNNER_TEMP must NOT propagate on Windows — it points at the runner's on-host scratch and the harness owns TMPDIR"
        );
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_drops_runner_workspace_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("RUNNER_WORKSPACE", r"C:\fake\workspace")]);
        assert!(
            !env.contains_key("RUNNER_WORKSPACE"),
            "RUNNER_WORKSPACE must NOT propagate on Windows — host workflow state, not identity"
        );
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_drops_github_workspace_for_hermeticity() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("GITHUB_WORKSPACE", r"C:\fake\gh_workspace")]);
        assert!(
            !env.contains_key("GITHUB_WORKSPACE"),
            "GITHUB_WORKSPACE must NOT propagate on Windows — points at the GH-runner-owned checkout, not the hermetic worktree"
        );
    }

    #[test]
    fn harness_env_injects_remap_path_prefix_for_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
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
    }

    #[test]
    fn harness_env_preserves_host_rustflags() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(
            tmp.path(),
            &[("RUSTFLAGS", "-C linker=link.exe -C link-arg=/DEBUG")],
        );
        let rf = env.get("RUSTFLAGS").unwrap();
        assert!(
            rf.contains("-C linker=link.exe"),
            "host RUSTFLAGS must survive the harness append. got={rf}"
        );
        assert!(
            rf.contains("--remap-path-prefix="),
            "remap-path-prefix must be appended even when host RUSTFLAGS is set. got={rf}"
        );
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
    /// over `RUSTFLAGS`, so the per-target value REPLACES (not merges
    /// with) the global.
    #[test]
    fn harness_env_injects_msvc_determinism_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[]);
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
    }

    /// Regression (the v0.11.3 PE TimeDateStamp drift): when the host is
    /// windows-msvc, global RUSTFLAGS must ALSO carry the MSVC determinism
    /// flags so the host build (e.g. `cargo run --release` invoked by a
    /// `before:` hook, which has no `--target` and therefore reads global
    /// RUSTFLAGS) lands a byte-stable `target/release/anodizer.exe`.
    ///
    /// Linux-runnable: the host-windows decision is INJECTED, not read from
    /// `cfg!(windows)`. The original guard was `#[cfg(windows)]`-gated and
    /// so never ran on Linux CI — which is precisely why the regression
    /// shipped undetected. This now runs on every host.
    #[test]
    fn harness_env_injects_msvc_flags_into_global_rustflags_for_windows_host() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with_host(tmp.path(), &[], true);
        let rf = env.get("RUSTFLAGS").expect(
            "RUSTFLAGS must be set for a windows-msvc host so host builds (no --target) are reproducible",
        );
        for needle in [
            "-C codegen-units=1",
            "-C link-arg=/Brepro",
            "-C link-arg=/OPT:NOICF",
            "-C link-arg=/INCREMENTAL:NO",
            "-C link-arg=/DEBUG:NONE",
            "-C strip=symbols",
        ] {
            assert!(
                rf.contains(needle),
                "global RUSTFLAGS must carry `{needle}` for a windows host. got={rf}"
            );
        }
        assert!(
            rf.contains("--remap-path-prefix="),
            "global RUSTFLAGS must also carry --remap-path-prefix. got={rf}"
        );
        assert_eq!(
            rf.matches("/Brepro").count(),
            1,
            "/Brepro must appear exactly once in global RUSTFLAGS. got={rf}"
        );
    }

    /// Mirror of the above: a NON-windows host must NOT get the
    /// MSVC-linker-only flags injected into global RUSTFLAGS — `/Brepro`
    /// makes lld / ld error, so a Linux/macOS host build would break.
    #[test]
    fn harness_env_omits_global_msvc_flags_for_non_windows_host() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with_host(tmp.path(), &[], false);
        // Global RUSTFLAGS is only set when a base existed (remap rules), and
        // must never carry /Brepro on a non-windows host.
        if let Some(rf) = env.get("RUSTFLAGS") {
            assert!(
                !rf.contains("/Brepro"),
                "non-windows host must NOT carry MSVC-only /Brepro in global RUSTFLAGS. got={rf}"
            );
        }
        // The per-target windows entries still exist (they're target-keyed,
        // valid for cross-building Windows from this host).
        assert!(
            env.contains_key("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"),
            "per-target windows-msvc RUSTFLAGS must still be injected regardless of host"
        );
    }

    /// Regression: the inherit-everything pass must DROP every rustflags-
    /// family host var. A host `CARGO_ENCODED_RUSTFLAGS` (Cargo exports it
    /// into the `cargo run`-launched anodizer process) out-precedences the
    /// harness's deliberately-injected `/Brepro`, silently dropping it and
    /// producing a non-reproducible binary.
    #[test]
    #[cfg(windows)]
    fn windows_env_should_drop_rustflags_family() {
        for key in [
            "CARGO_ENCODED_RUSTFLAGS",
            "cargo_encoded_rustflags",
            "RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
            "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS",
            "cargo_target_x86_64_pc_windows_msvc_rustflags",
        ] {
            assert!(
                windows_env_should_drop(key),
                "{key} is a rustflags-family var that out-precedences /Brepro and MUST be dropped"
            );
        }
        for key in ["CARGO_HOME", "PATH", "CARGO_TARGET_DIR"] {
            assert!(
                !windows_env_should_drop(key),
                "{key} is unrelated to rustflags and MUST NOT be dropped by the rustflags sweep"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn harness_env_windows_keeps_runner_os_allow_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let env = build_with(tmp.path(), &[("RUNNER_OS", "Windows")]);
        assert_eq!(
            env.get("RUNNER_OS").map(String::as_str),
            Some("Windows"),
            "RUNNER_OS is on the identity allow-list and MUST propagate even though the namespace gate would otherwise drop it"
        );
    }
}
