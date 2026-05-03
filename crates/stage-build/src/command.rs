use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anodizer_core::config::CrossStrategy;
use anodizer_core::util::find_binary;

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
    if find_binary("cargo-zigbuild") {
        return CrossStrategy::Zigbuild;
    }
    if find_binary("cross") {
        return CrossStrategy::Cross;
    }
    CrossStrategy::Cargo
}

/// Target-aware variant of [`detect_cross_strategy`].
///
/// `cargo` is the right choice whenever the target's OS is the same as the
/// host's OS, because the host's native compiler already knows how to
/// emit binaries for that OS on every supported arch:
///
/// - **macOS host → any apple-darwin target**: clang is a universal
///   cross-compiler across Apple architectures (x86_64, aarch64) and the
///   SDK is already on disk, so `cargo build --target …-apple-darwin`
///   works natively. zigbuild on macOS historically mis-handles the
///   framework paths in large link lines and fails on x86_64-apple-darwin
///   when run from an arm64 runner.
/// - **Linux host → any *-linux-gnu target with matching libc**: cargo
///   links with the host's gcc; cross-arch Linux needs a multilib package
///   or the `cross` container, so same-OS-different-arch Linux still
///   benefits from zigbuild/cross. Keep the existing auto behaviour.
/// - **Windows host → any *-pc-windows-* target**: MSVC cl/link handles
///   both msvc x86_64 and aarch64 via the VS install, no zig needed.
///
/// Only fall back to the cross tooling when actually crossing OS boundaries
/// (Linux → Windows, Linux → darwin, etc.).
pub(crate) fn detect_cross_strategy_for_target(target: &str) -> CrossStrategy {
    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();

    // Exact host match — always cargo.
    if !host.is_empty() && target == host {
        return CrossStrategy::Cargo;
    }

    // Same-OS, different-arch — cargo when the host's native toolchain
    // can handle the target without external cross tooling. Applies to
    // Apple (clang is universal across apple arches) and Windows (MSVC
    // handles every windows arch). Linux stays on the cross tooling
    // because same-OS cross-arch still needs a gcc multilib or similar.
    if !host.is_empty() && same_apple_family(&host, target) {
        return CrossStrategy::Cargo;
    }
    if !host.is_empty() && same_windows_family(&host, target) {
        return CrossStrategy::Cargo;
    }

    detect_cross_strategy()
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
    // supplied one, so native targets always use cargo even if
    // cargo-zigbuild or cross are available (zig has known issues
    // linking for Apple hosts, cross can't cross to the same host).
    let resolved = if *strategy == CrossStrategy::Auto {
        match target {
            Some(t) => detect_cross_strategy_for_target(t),
            None => detect_cross_strategy(),
        }
    } else {
        strategy.clone()
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
// build_command
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_command(
    binary: &str,
    crate_path: &str,
    target: &str,
    strategy: &CrossStrategy,
    flags: &[String],
    features: &[String],
    no_default_features: bool,
    env: &HashMap<String, String>,
    cross_tool: Option<&str>,
    command_override: Option<&str>,
) -> BuildCommand {
    let (program, subcommand) =
        resolve_build_program(strategy, cross_tool, command_override, Some(target));

    // The subcommand may contain spaces (e.g. "auditable build"), split into separate args
    let mut args: Vec<String> = subcommand
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        "--bin".to_string(),
        binary.to_string(),
        "--target".to_string(),
        target.to_string(),
    ]);

    // Append flags (one argv token per entry — quoted shell args survive).
    args.extend(flags.iter().cloned());

    // Features
    if !features.is_empty() {
        args.push("--features".to_string());
        args.push(features.join(","));
    }

    if no_default_features {
        args.push("--no-default-features".to_string());
    }

    BuildCommand {
        program,
        args,
        env: env.clone(),
        cwd: PathBuf::from(crate_path),
    }
}

// ---------------------------------------------------------------------------
// build_lib_command
// ---------------------------------------------------------------------------

/// Build command for library targets (cdylib, staticlib, etc.).
/// Uses `--lib` instead of `--bin`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_lib_command(
    crate_path: &str,
    target: &str,
    strategy: &CrossStrategy,
    flags: &[String],
    features: &[String],
    no_default_features: bool,
    env: &HashMap<String, String>,
    cross_tool: Option<&str>,
    command_override: Option<&str>,
) -> BuildCommand {
    let (program, subcommand) =
        resolve_build_program(strategy, cross_tool, command_override, Some(target));

    // The subcommand may contain spaces (e.g. "auditable build"), split into separate args
    let mut args: Vec<String> = subcommand
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        "--lib".to_string(),
        "--target".to_string(),
        target.to_string(),
    ]);

    // Append flags (one argv token per entry).
    args.extend(flags.iter().cloned());

    // Features
    if !features.is_empty() {
        args.push("--features".to_string());
        args.push(features.join(","));
    }

    if no_default_features {
        args.push("--no-default-features".to_string());
    }

    BuildCommand {
        program,
        args,
        env: env.clone(),
        cwd: PathBuf::from(crate_path),
    }
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
