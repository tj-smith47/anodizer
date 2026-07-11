use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anodizer_core::config::CrossStrategy;
use anodizer_core::tool_detect::on_path;

// ---------------------------------------------------------------------------
// BuildCommand — a description of the command to run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BuildCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Working directory for the command (crate root)
    pub cwd: PathBuf,
}

// ---------------------------------------------------------------------------
// detect_cross_strategy
// ---------------------------------------------------------------------------

pub(crate) fn detect_cross_strategy() -> CrossStrategy {
    detect_cross_strategy_impl(zigbuild_available(), on_path("cross"))
}

/// True when a zigbuild invocation can actually run: `cargo-zigbuild` on
/// PATH AND a reachable zig toolchain behind it. Probing only the cargo
/// subcommand would select a strategy that fails at spawn time on hosts
/// where zig itself is missing.
pub(crate) fn zigbuild_available() -> bool {
    on_path("cargo-zigbuild") && zig_available()
}

/// Whether the zig toolchain cargo-zigbuild shells out to is reachable.
/// cargo-zigbuild resolves zig as the `zig` binary on PATH or, failing
/// that, the pip-installed `ziglang` wheel driven via `python3 -m ziglang`
/// (`python` on hosts without a `python3` shim); both probes are mirrored
/// here so zigbuild is only chosen when an invocation would succeed.
/// Cached for the process lifetime: strategy resolution runs per build
/// job and the wheel probe spawns an interpreter.
fn zig_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if on_path("zig") {
            return true;
        }
        ["python3", "python"].iter().any(|py| {
            std::process::Command::new(py)
                .args(["-c", "import ziglang"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    })
}

/// Tool-availability core of [`detect_cross_strategy`], with the PATH probes
/// injected so the preference order is testable without touching PATH.
pub(crate) fn detect_cross_strategy_impl(
    zigbuild_available: bool,
    cross_available: bool,
) -> CrossStrategy {
    if zigbuild_available {
        return CrossStrategy::Zigbuild;
    }
    if cross_available {
        return CrossStrategy::Cross;
    }
    CrossStrategy::Cargo
}

/// Target-aware variant of [`detect_cross_strategy`].
///
/// Strategy choice depends on the host/target family, not just on which
/// tools are installed:
///
/// - **macOS host → any apple-darwin target**: clang is a universal
///   cross-compiler across Apple architectures (x86_64, aarch64) and the
///   SDK is already on disk, so `cargo build --target …-apple-darwin`
///   works natively. zigbuild on macOS historically mis-handles the
///   framework paths in large link lines and fails on x86_64-apple-darwin
///   when run from an arm64 runner.
/// - **Any host → *-linux-gnu target**: zigbuild whenever cargo-zigbuild
///   is available, including for the exact host triple — zig's bundled
///   libc keeps the binary's glibc floor independent of the build
///   machine's glibc, so a CI runner image upgrade cannot silently raise
///   the released binary's glibc requirement. Without zigbuild, the host
///   triple falls back to native cargo (local dev) and cross-arch falls
///   back to `cross`/cargo.
/// - **Windows host → any *-pc-windows-* target**: MSVC cl/link handles
///   both msvc x86_64 and aarch64 via the VS install, no zig needed.
///
/// Only fall back to the cross tooling when actually crossing OS boundaries
/// (Linux → Windows, Linux → darwin, etc.).
pub(crate) fn detect_cross_strategy_for_target(target: &str) -> CrossStrategy {
    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    detect_cross_strategy_for_target_impl(&host, target, zigbuild_available(), on_path("cross"))
}

/// Decision core of [`detect_cross_strategy_for_target`], with the host
/// triple and tool-availability probes injected so every host/target/tool
/// combination is testable on any machine.
pub(crate) fn detect_cross_strategy_for_target_impl(
    host: &str,
    target: &str,
    zigbuild_available: bool,
    cross_available: bool,
) -> CrossStrategy {
    // glibc-linked Linux targets route through zigbuild whenever it is
    // available — even when the target is the exact host triple. Native
    // cargo links the build machine's ambient glibc, so the binary's glibc
    // floor silently tracks the CI runner image (ubuntu-24.04 produces a
    // GLIBC_2.39 requirement, uninstallable on Debian 12 / Ubuntu 22.04).
    // zig ships its own libc headers, keeping the floor hermetic and
    // independent of runner upgrades.
    //
    // musl Linux triples route through zigbuild for a different reason:
    // anodizer ships the apk package as a musl binary, and the glibc CI
    // release host always cross-compiles musl. Plain cargo then dies in
    // cc-rs (no musl cross C toolchain on stock runners). cargo-zigbuild
    // bundles musl headers for x86_64 and aarch64 alike, so it cross-builds
    // musl cleanly without `cross` or musl-tools.
    if (is_linux_gnu(target) || is_linux_musl(target)) && zigbuild_available {
        return CrossStrategy::Zigbuild;
    }

    // Exact host match (only non-glibc targets reach this point, plus
    // linux-gnu without zigbuild installed) — native cargo.
    if !host.is_empty() && target == host {
        return CrossStrategy::Cargo;
    }

    // Same-OS, different-arch — cargo when the host's native toolchain
    // can handle the target without external cross tooling. Applies to
    // Apple (clang is universal across apple arches) and Windows (MSVC
    // handles every windows arch). Linux stays on the cross tooling
    // because same-OS cross-arch still needs a gcc multilib or similar.
    if !host.is_empty() && same_apple_family(host, target) {
        return CrossStrategy::Cargo;
    }
    if !host.is_empty() && same_windows_family(host, target) {
        return CrossStrategy::Cargo;
    }

    detect_cross_strategy_impl(zigbuild_available, cross_available)
}

/// Resolve the effective strategy for `target`: `Auto` resolves via the
/// host/target/tool-availability probes, anything else is taken verbatim.
pub(crate) fn resolved_strategy_for_target(
    strategy: &CrossStrategy,
    target: &str,
) -> CrossStrategy {
    if *strategy == CrossStrategy::Auto {
        detect_cross_strategy_for_target(target)
    } else {
        strategy.clone()
    }
}

/// Warning for a cross-arch `*-linux-gnu` build about to run under plain
/// `cargo build`. Without cargo-zigbuild or cross, cc-rs resolves the
/// target C compiler from the system (e.g. `aarch64-linux-gnu-gcc`),
/// which stock CI runners don't ship — the first native-code dependency
/// (ring, libgit2, ...) then dies with an opaque `ToolNotFound` deep in a
/// build script. Naming the routing decision up front turns that into an
/// actionable message. Returns `None` when the resolved strategy is not
/// plain cargo, the target is not glibc Linux, or the target is the host
/// triple (native builds need no cross cc).
pub(crate) fn cross_gnu_cargo_fallback_warning(
    host: &str,
    target: &str,
    resolved: &CrossStrategy,
) -> Option<String> {
    if *resolved != CrossStrategy::Cargo {
        return None;
    }
    let gcc = cross_gnu_cargo_gcc(host, target)?;
    Some(format!(
        "cross gnu target '{target}' resolved to plain cargo (cargo-zigbuild/cross not \
         installed); native-code dependencies will need a system cross C toolchain \
         (e.g. `{gcc}`) — install cargo-zigbuild + zig for a hermetic \
         cross build"
    ))
}

/// Outcome of [`cross_gnu_cargo_fallback_gate`]: how loudly to surface a
/// cross-arch `*-linux-gnu` target resolving to plain `cargo build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrossGnuFallback {
    /// Working (or plausibly operator-managed) setup — surface the routing
    /// decision but let the build proceed.
    Warn(String),
    /// Provably doomed: no cross gcc on PATH and no operator override.
    /// Failing at planning time saves the minutes cargo would burn before
    /// cc-rs dies with an opaque `ToolNotFound` in the first native-code
    /// dependency.
    Error(String),
}

/// Escalation decision for the cross-gnu plain-cargo fallback. Returns
/// `None` in exactly the cases [`cross_gnu_cargo_fallback_warning`] is
/// silent (not plain cargo, not glibc Linux, native target, unknown host).
/// Otherwise escalates the warning to a hard error iff the implied cross
/// gcc is absent from PATH AND no operator override signal is present —
/// a setup where the build cannot succeed. Pure: the PATH probe, process
/// env, and `.cargo/config*` linker check are all injected.
pub(crate) fn cross_gnu_cargo_fallback_gate(
    host: &str,
    target: &str,
    resolved: &CrossStrategy,
    build_env: &HashMap<String, String>,
    env: &dyn anodizer_core::EnvSource,
    gcc_on_path: &dyn Fn(&str) -> bool,
    cargo_config_mentions_linker: bool,
) -> Option<CrossGnuFallback> {
    let warn_msg = cross_gnu_cargo_fallback_warning(host, target, resolved)?;
    // The warning implies a cross gcc exists for this host/target pair.
    let gcc = cross_gnu_cargo_gcc(host, target)?;
    if gcc_on_path(&gcc)
        || cargo_config_mentions_linker
        || cross_gnu_operator_override(target, build_env, env)
    {
        return Some(CrossGnuFallback::Warn(warn_msg));
    }
    Some(CrossGnuFallback::Error(format!(
        "cross gnu target '{target}' resolved to plain cargo (cargo-zigbuild/cross not \
         installed) and `{gcc}` was not found on PATH — native-code dependencies cannot \
         compile, so the build is guaranteed to fail after minutes of compilation; \
         install cargo-zigbuild + zig for a hermetic cross build, or install the system \
         cross toolchain / set an explicit linker (e.g. CARGO_TARGET_{triple}_LINKER)",
        triple = env_triple(target),
    )))
}

/// `<triple>` uppercased with `-`/`.` mapped to `_`, the spelling cargo's
/// per-target env vars use (`CARGO_TARGET_<TRIPLE>_LINKER`). Glibc-pinned
/// suffixes are stripped first — plain cargo only ever sees the bare triple.
fn env_triple(target: &str) -> String {
    let (bare, _) = crate::validation::strip_glibc_suffix(target);
    bare.to_uppercase().replace(['-', '.'], "_")
}

/// True when the operator has visibly taken manual control of the target's
/// C toolchain / linker, via the process env or the build config's own env
/// map — any such signal downgrades the doomed-build error back to the
/// warning, so exotic-but-working local setups are never false-blocked.
fn cross_gnu_operator_override(
    target: &str,
    build_env: &HashMap<String, String>,
    env: &dyn anodizer_core::EnvSource,
) -> bool {
    let (bare, _) = crate::validation::strip_glibc_suffix(target);
    let lookup = |key: &str| env.var(key).or_else(|| build_env.get(key).cloned());

    // Explicit per-target linker.
    if lookup(&format!("CARGO_TARGET_{}_LINKER", env_triple(target))).is_some() {
        return true;
    }
    // A linker override buried in rustflags (encoded form uses \x1f
    // separators, but plain containment still matches `linker=`).
    for key in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        if lookup(key).is_some_and(|v| v.contains("linker=")) {
            return true;
        }
    }
    // cc-rs C-compiler overrides: TARGET_CC, CC_<triple> in both the
    // dashed and underscored spellings cc-rs accepts, plus the classic
    // CROSS_COMPILE prefix.
    let cc_keys = [
        "TARGET_CC".to_string(),
        "CROSS_COMPILE".to_string(),
        format!("CC_{bare}"),
        format!("CC_{}", bare.replace('-', "_")),
    ];
    cc_keys.iter().any(|key| lookup(key).is_some())
}

/// Cheap containment check: does any `.cargo/config.toml` / `.cargo/config`
/// directly under one of `dirs` mention `linker` at all? Deliberately
/// over-permissive — if the file talks about linkers, assume the setup is
/// operator-managed and warn instead of hard-failing.
pub(crate) fn cargo_config_mentions_linker(dirs: &[&Path]) -> bool {
    dirs.iter().any(|dir| {
        ["config.toml", "config"].iter().any(|name| {
            std::fs::read_to_string(dir.join(".cargo").join(name))
                .is_ok_and(|content| content.contains("linker"))
        })
    })
}

/// The system cross C compiler a plain-`cargo` build of `target` would resolve
/// through cc-rs: `{arch}-linux-gnu-gcc`, where `arch` is the first `-`-split
/// component of the glibc-suffix-stripped triple (e.g. `aarch64-linux-gnu-gcc`
/// for `aarch64-unknown-linux-gnu`).
///
/// Returns `None` when no cross gcc is implied: the target is not glibc Linux,
/// the host triple is unknown, or the target IS the host triple (a native build
/// links the ambient toolchain, no cross gcc). Shared by
/// [`cross_gnu_cargo_fallback_warning`] (the runtime warning) and the
/// `tools`-emit cross-toolchain self-report so both name the same binary.
pub(crate) fn cross_gnu_cargo_gcc(host: &str, target: &str) -> Option<String> {
    if !is_linux_gnu(target) || host.is_empty() {
        return None;
    }
    // Glibc-pinned spellings (`x86_64-unknown-linux-gnu.2.17`) of the host
    // triple are still native builds.
    let (bare_target, _) = crate::validation::strip_glibc_suffix(target);
    if bare_target == host {
        return None;
    }
    let arch = bare_target.split('-').next().unwrap_or(bare_target);
    Some(format!("{arch}-linux-gnu-gcc"))
}

/// True for glibc-linked Linux triples: `*-linux-gnu`, ABI-suffixed forms
/// like `*-linux-gnueabihf`, and glibc-pinned spellings like
/// `x86_64-unknown-linux-gnu.2.17`. musl triples return false.
pub(crate) fn is_linux_gnu(target: &str) -> bool {
    target.contains("-linux-gnu")
}

/// True for musl-linked Linux triples: `*-linux-musl` and ABI-suffixed forms
/// like `*-linux-musleabihf`. glibc triples return false.
pub(crate) fn is_linux_musl(target: &str) -> bool {
    target.contains("-linux-musl")
}

/// True when both triples target Apple's Darwin kernel. Matches
/// *-apple-darwin, *-apple-ios*, *-apple-tvos*, *-apple-watchos* on either side.
pub(crate) fn same_apple_family(host: &str, target: &str) -> bool {
    host.contains("-apple-") && target.contains("-apple-")
}

/// True when both triples target Windows (any arch, any subsystem).
pub(crate) fn same_windows_family(host: &str, target: &str) -> bool {
    host.contains("-windows-") && target.contains("-windows-")
}

// ---------------------------------------------------------------------------
// resolve_build_program — shared cross_tool / strategy resolution
// ---------------------------------------------------------------------------

/// Resolve the build program and subcommand from the cross strategy and
/// optional cross_tool override. When `cross_tool` is set it takes precedence
/// over any strategy — the tool is used directly with "build" as the subcommand.
///
/// When `command_override` is set (from `BuildConfig.command`), it replaces
/// the auto-detected subcommand. For example, `command: "auditable build"`
/// produces `cargo auditable build` instead of `cargo build`.
pub(crate) fn resolve_build_program(
    strategy: &CrossStrategy,
    cross_tool: Option<&str>,
    command_override: Option<&str>,
    target: Option<&str>,
) -> (String, String) {
    if let Some(tool) = cross_tool {
        let subcmd = command_override.unwrap_or("build").to_string();
        return (tool.to_string(), subcmd);
    }

    // Resolve Auto strategy at runtime. Target-aware when the caller
    // supplied one: native darwin/windows/musl targets use cargo even if
    // cargo-zigbuild or cross are available (zig has known issues linking
    // for Apple hosts, cross can't cross to the same host), while
    // linux-gnu targets prefer zigbuild for a hermetic glibc floor.
    let resolved = match target {
        Some(t) => resolved_strategy_for_target(strategy, t),
        None if *strategy == CrossStrategy::Auto => detect_cross_strategy(),
        None => strategy.clone(),
    };

    let (prog, default_subcmd) = match resolved {
        CrossStrategy::Zigbuild => ("cargo".to_string(), "zigbuild"),
        CrossStrategy::Cross => ("cross".to_string(), "build"),
        // Cargo and Auto (already resolved above)
        _ => ("cargo".to_string(), "build"),
    };

    let subcmd = command_override.unwrap_or(default_subcmd).to_string();
    (prog, subcmd)
}

// ---------------------------------------------------------------------------
// BuildContext + helpers
// ---------------------------------------------------------------------------

/// Per-call context shared by [`build_command`] and [`build_lib_command`].
///
/// Bundles every parameter that's identical across the bin and lib paths
/// (toolchain selection, cargo flags, feature flags, env, target triple,
/// crate path) so each public entry point only has to supply the
/// target-selector args (`--bin <name>` vs `--lib`). All fields are
/// borrowed; the struct is short-lived.
pub(crate) struct BuildContext<'a> {
    pub crate_path: &'a str,
    pub target: &'a str,
    pub strategy: &'a CrossStrategy,
    pub flags: &'a [String],
    pub features: &'a [String],
    pub no_default_features: bool,
    pub env: &'a HashMap<String, String>,
    pub cross_tool: Option<&'a str>,
    pub command_override: Option<&'a str>,
}

/// Internal helper that does the shared cargo-invocation construction. Takes
/// a `target_selector` (`["--bin", binary, "--target", target]` for bin
/// builds or `["--lib", "--target", target]` for lib builds) plus the
/// invariant [`BuildContext`] and assembles the full `BuildCommand`.
///
/// Centralising the body here means every change to flag handling,
/// feature handling, or `--no-default-features` semantics happens in one
/// place — the bin and lib paths can never drift apart.
fn build_target_command(target_selector: &[&str], ctx: &BuildContext<'_>) -> BuildCommand {
    let (program, subcommand) = resolve_build_program(
        ctx.strategy,
        ctx.cross_tool,
        ctx.command_override,
        Some(ctx.target),
    );

    // The subcommand may contain spaces (e.g. "auditable build"), split into separate args
    let mut args: Vec<String> = subcommand
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    args.extend(target_selector.iter().map(|s| s.to_string()));

    // Append flags (one argv token per entry — quoted shell args survive).
    args.extend(ctx.flags.iter().cloned());

    // Features
    if !ctx.features.is_empty() {
        args.push("--features".to_string());
        args.push(ctx.features.join(","));
    }

    if ctx.no_default_features {
        args.push("--no-default-features".to_string());
    }

    BuildCommand {
        program,
        args,
        env: ctx.env.clone(),
        cwd: PathBuf::from(ctx.crate_path),
    }
}

// ---------------------------------------------------------------------------
// build_command — `cargo build --bin <binary>`
// ---------------------------------------------------------------------------

pub(crate) fn build_command(binary: &str, ctx: &BuildContext<'_>) -> BuildCommand {
    build_target_command(&["--bin", binary, "--target", ctx.target], ctx)
}

// ---------------------------------------------------------------------------
// build_lib_command — `cargo build --lib`
// ---------------------------------------------------------------------------

/// Build command for library targets (cdylib, staticlib, etc.).
/// Uses `--lib` instead of `--bin`.
pub(crate) fn build_lib_command(ctx: &BuildContext<'_>) -> BuildCommand {
    build_target_command(&["--lib", "--target", ctx.target], ctx)
}

// ---------------------------------------------------------------------------
// detect_crate_type
// ---------------------------------------------------------------------------

/// Check if a crate has a binary target.
///
/// Three probes, ordered cheapest-first:
/// 1. `src/main.rs` exists (the cargo default-bin convention).
/// 2. `[[bin]]` section in `Cargo.toml`.
/// 3. Any `*.rs` file under `src/bin/` (cargo auto-discovers these as
///    additional bin targets even when `[[bin]]` is omitted — common in
///    multi-binary crates and a real-world miss before this branch was
///    added).
///
/// Returns false for library-only crates.
///
/// Limitation: probe (3) does not honour `[package].autobins = false`. A crate
/// that explicitly opts out of `src/bin/` autodiscovery via that flag, AND has
/// `*.rs` files in `src/bin/`, AND does not declare any `[[bin]]` block, will
/// be misclassified as having a binary target. The clean way to opt out is to
/// declare `[[bin]]` explicitly (which probe (2) honours) — `autobins = false`
/// without a replacement `[[bin]]` is rare enough that we don't parse the flag
/// here.
pub(crate) fn crate_has_binary_target(crate_path: &str) -> bool {
    let path = Path::new(crate_path);
    // Check for src/main.rs
    if path.join("src/main.rs").exists() {
        return true;
    }
    // Check for [[bin]] section in Cargo.toml
    let cargo_toml = path.join("Cargo.toml");
    if let Ok(content) = std::fs::read_to_string(&cargo_toml)
        && let Ok(doc) = content.parse::<toml_edit::DocumentMut>()
        && let Some(bins) = doc.get("bin")
        && let Some(arr) = bins.as_array_of_tables()
        && !arr.is_empty()
    {
        return true;
    }
    // Check for src/bin/<name>.rs auto-discovered targets.
    if let Ok(mut entries) = path.join("src/bin").read_dir()
        && entries.any(|e| {
            e.ok()
                .is_some_and(|x| x.path().extension().is_some_and(|ext| ext == "rs"))
        })
    {
        return true;
    }
    false
}

/// Read a crate's Cargo.toml and return the first `crate-type` from [lib],
/// if present (e.g. "cdylib", "staticlib", "rlib").
pub(crate) fn detect_crate_type(crate_path: &str) -> Option<String> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path).ok()?;
    let doc = content.parse::<toml_edit::DocumentMut>().ok()?;
    let lib = doc.get("lib")?;
    let crate_types = lib.get("crate-type").or_else(|| lib.get("crate_type"))?;
    let arr = crate_types.as_array()?;
    arr.get(0).and_then(|v| v.as_str()).map(|s| s.to_string())
}
