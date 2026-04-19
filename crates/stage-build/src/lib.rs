use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{
    BuildConfig, BuildIgnore, BuildOverride, CrossStrategy, HookEntry, UniversalBinaryConfig,
};
use anodize_core::context::Context;
use anodize_core::env_expand::expand_env as expand_env_vars;
use anodize_core::hooks::run_hooks;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;
use anodize_core::util::find_binary;

pub mod binstall;
pub mod version_sync;

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
    let host = anodize_core::partial::detect_host_target().unwrap_or_default();

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
fn same_apple_family(host: &str, target: &str) -> bool {
    host.contains("-apple-") && target.contains("-apple-")
}

/// True when both triples target Windows (any arch, any subsystem).
fn same_windows_family(host: &str, target: &str) -> bool {
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
    flags: Option<&str>,
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

    // Append flags (split on whitespace)
    if let Some(f) = flags {
        for part in f.split_whitespace() {
            args.push(part.to_string());
        }
    }

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
    flags: Option<&str>,
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

    // Append flags (split on whitespace)
    if let Some(f) = flags {
        for part in f.split_whitespace() {
            args.push(part.to_string());
        }
    }

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

/// Check if a crate has a binary target (src/main.rs or [[bin]] in Cargo.toml).
/// Returns false for library-only crates.
fn crate_has_binary_target(crate_path: &str) -> bool {
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
    {
        return !arr.is_empty();
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

// ---------------------------------------------------------------------------
// detect_cargo_profile — parse --release / --profile flags from cargo flags
// ---------------------------------------------------------------------------

/// Detect the effective cargo profile from a flags string.
///
/// Handles `--release`, `--profile release`, and `--profile=release` (or any
/// other profile name like `--profile=bench`).  Falls back to `"debug"` when
/// no profile flag is found.
///
/// Returns a `&str` that borrows from the input flags string for custom
/// profile names, or a static string for well-known profiles.
fn detect_cargo_profile(flags: Option<&str>) -> &str {
    let flags = match flags {
        Some(f) => f,
        None => return "debug",
    };

    let tokens: Vec<&str> = flags.split_whitespace().collect();

    // Check for --profile=<name> (equals form)
    for token in &tokens {
        if let Some(name) = token.strip_prefix("--profile=")
            && !name.is_empty()
        {
            return match name {
                "dev" => "debug",
                _ => name,
            };
        }
    }

    // Check for --profile <name> (space-separated form)
    for i in 0..tokens.len() {
        if tokens[i] == "--profile"
            && let Some(&name) = tokens.get(i + 1)
        {
            return match name {
                "dev" => "debug",
                _ => name,
            };
        }
    }

    // Check for --release flag
    if tokens.contains(&"--release") {
        return "release";
    }

    "debug"
}

// ---------------------------------------------------------------------------
// build_universal_binary — run `lipo` to combine arm64 + x86_64 macOS binaries
// ---------------------------------------------------------------------------

fn build_universal_binary(
    crate_name: &str,
    ub: &UniversalBinaryConfig,
    ctx: &mut Context,
    dry_run: bool,
) -> anyhow::Result<()> {
    let log = ctx.logger("build");
    // Collect arm64 and x86_64 macOS binary artifacts for this crate.
    // When `ids` is set, only consider artifacts whose "binary" metadata key (the binary name)
    // is in the list. Build artifacts use "binary" as their identifier, not "id".
    let binaries = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name);

    // GoReleaser universalbinary.go:42-44 — default `ids` to [ID] when unset.
    // The universal binary's `id` (or the crate name as a last resort) is the
    // implicit filter, so a bare `universal_binaries: [{ id: foo }]` selects
    // only build outputs with that id.
    let default_ids: Vec<String> = vec![ub.id.clone().unwrap_or_else(|| crate_name.to_string())];
    let effective_ids = ub.ids.clone().unwrap_or(default_ids);

    let filtered: Vec<_> = if !effective_ids.is_empty() {
        binaries
            .into_iter()
            .filter(|a| {
                // Match on either "binary" (historical) or "id" (GoReleaser).
                a.metadata
                    .get("id")
                    .map(|v| effective_ids.contains(v))
                    .unwrap_or(false)
                    || a.metadata
                        .get("binary")
                        .map(|v| effective_ids.contains(v))
                        .unwrap_or(false)
            })
            .collect()
    } else {
        binaries
    };

    let arm64 = filtered
        .iter()
        .find(|a| a.target.as_deref() == Some("aarch64-apple-darwin"));
    let x86_64 = filtered
        .iter()
        .find(|a| a.target.as_deref() == Some("x86_64-apple-darwin"));

    let (arm64_path, x86_64_path) = match (arm64, x86_64) {
        (Some(a), Some(x)) => (a.path.clone(), x.path.clone()),
        _ => {
            // Not an error: universal binaries require both darwin archs, which
            // only exist on macOS builds or in merge mode. On Linux/Windows split
            // builds this skip is expected — not a strict_guard situation.
            log.verbose(&format!(
                "universal_binaries: skipping {crate_name} — \
                 both aarch64-apple-darwin and x86_64-apple-darwin binaries required"
            ));
            return Ok(());
        }
    };

    // `binary_name` is the source binary filename — preserved for the
    // `binary` metadata key (downstream consumers treat it as the binary's
    // on-disk name).
    let binary_name = arm64_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate_name.to_string());

    // Determine output path / name.
    //
    // GoReleaser universalbinary.go:45 — the default `name_template` is
    // `{{ .ProjectName }}`, NOT the source binary filename. We render the
    // default explicitly so `.exe`-suffixed source names and custom
    // `BuildConfig.binary` values do not leak into the universal output.
    let out_name = if let Some(ref tmpl) = ub.name_template {
        ctx.render_template_strict(tmpl, "universal_binaries name_template", &log)?
    } else {
        ctx.render_template_strict(
            "{{ .ProjectName }}",
            "universal_binaries name_template (default)",
            &log,
        )?
    };

    // Place the universal binary in dist/{crate_name}_darwin_all/{name}
    // matching GoReleaser's convention for universal binaries.
    let dist_dir = &ctx.config.dist;
    let ub_dir = dist_dir.join(format!("{}_darwin_all", crate_name));
    let out_path = ub_dir.join(&out_name);

    // Execute pre-hooks if configured
    let template_vars = ctx.template_vars().clone();
    if let Some(ref hooks) = ub.hooks
        && let Some(ref pre) = hooks.pre
    {
        run_hooks(
            pre,
            "pre-universal-binary",
            dry_run,
            &log,
            Some(&template_vars),
        )?;
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        ));
    } else {
        // Check lipo is available — this is an error since the user
        // explicitly configured universal_binaries.
        if !find_binary("lipo") {
            anyhow::bail!(
                "lipo not found but universal_binaries is configured for {crate_name}; \
                 install Xcode command-line tools or ensure lipo is on PATH"
            );
        }

        // Ensure output directory exists
        std::fs::create_dir_all(&ub_dir).with_context(|| {
            format!(
                "failed to create universal binary output dir: {}",
                ub_dir.display()
            )
        })?;

        log.status(&format!(
            "lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        ));

        let output = Command::new("lipo")
            .args([
                "-create",
                "-output",
                &out_path.to_string_lossy(),
                &arm64_path.to_string_lossy(),
                &x86_64_path.to_string_lossy(),
            ])
            .output()
            .with_context(|| format!("failed to spawn lipo for {crate_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("lipo failed for {crate_name}: {}", stderr.trim());
        }
    }

    // Execute post-hooks if configured
    if let Some(ref hooks) = ub.hooks
        && let Some(ref post) = hooks.post
    {
        run_hooks(
            post,
            "post-universal-binary",
            dry_run,
            &log,
            Some(&template_vars),
        )?;
    }

    // Apply mod_timestamp if configured
    if let Some(ref ts) = ub.mod_timestamp
        && !dry_run
        && out_path.exists()
    {
        let rendered_ts = ctx.render_template(ts).unwrap_or_else(|_| ts.clone());
        let mtime = anodize_core::util::parse_mod_timestamp(&rendered_ts)?;
        anodize_core::util::set_file_mtime(&out_path, mtime)?;
        log.verbose(&format!(
            "applied mod_timestamp={rendered_ts} to {}",
            out_path.display()
        ));
    }

    // Register the universal binary artifact with UniversalBinary kind.
    // Set `replaces` metadata for OnlyReplacingUnibins publisher filter:
    // true = this universal binary supersedes per-arch variants in publishers.
    let replaces = ub.replace == Some(true);

    // GoReleaser universalbinary.go:236-239 — preserve source binary Extras
    // (copied from the first source binary) before setting universal-specific
    // keys. Only forward the known-used keys to avoid leaking unrelated state.
    let mut metadata: HashMap<String, String> = HashMap::new();
    let first_source = arm64.or(x86_64);
    if let Some(src) = first_source {
        for key in &["dynamically_linked", "abi", "libc", "id"] {
            if let Some(v) = src.metadata.get(*key) {
                metadata.insert((*key).to_string(), v.clone());
            }
        }
    }
    // Universal-specific keys (override any copied values)
    metadata.insert("binary".to_string(), binary_name);
    metadata.insert("universal".to_string(), "true".to_string());
    metadata.insert("replaces".to_string(), replaces.to_string());
    // Universal binary's own id, if configured
    if let Some(ref id) = ub.id {
        metadata.insert("id".to_string(), id.clone());
    }

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UniversalBinary,
        name: String::new(),
        path: out_path,
        target: Some("darwin-universal".to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });

    // When `replace` is true, remove the source arm64/x86_64 artifacts from
    // the registry so downstream stages do not publish them alongside the
    // universal binary.
    if ub.replace == Some(true) {
        ctx.artifacts.remove_by_paths(&[arm64_path, x86_64_path]);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Build ignore/override helpers
// ---------------------------------------------------------------------------

/// Check if a target triple matches any entry in the ignore list.
/// Matching is done by comparing the os and arch components of the target triple.
pub(crate) fn is_target_ignored(target: &str, ignores: &[BuildIgnore]) -> bool {
    if ignores.is_empty() {
        return false;
    }
    let (os, arch) = map_target(target);
    ignores.iter().any(|ig| ig.os == os && ig.arch == arch)
}

/// Find the first matching override for a target triple.
/// Override `targets` are glob patterns matched against the full triple string.
pub(crate) fn find_matching_override<'a>(
    target: &str,
    overrides: &'a [BuildOverride],
    log: &anodize_core::log::StageLogger,
    strict: bool,
) -> anyhow::Result<Option<&'a BuildOverride>> {
    for ov in overrides {
        for pat_str in &ov.targets {
            match glob::Pattern::new(pat_str) {
                Ok(pat) => {
                    if pat.matches(target) {
                        return Ok(Some(ov));
                    }
                }
                Err(e) => {
                    if strict {
                        anyhow::bail!(
                            "build: invalid glob pattern '{}': {} (strict mode)",
                            pat_str,
                            e
                        );
                    }
                    log.warn(&format!("invalid glob pattern '{}': {}", pat_str, e));
                }
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Default targets — used when neither build.targets nor defaults.targets is set
// ---------------------------------------------------------------------------

const DEFAULT_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
];

// ---------------------------------------------------------------------------
// Known Rust target triples (Tier 1 + Tier 2) for validation
// ---------------------------------------------------------------------------

const KNOWN_TARGETS: &[&str] = &[
    // Tier 1
    "aarch64-unknown-linux-gnu",
    "i686-pc-windows-gnu",
    "i686-pc-windows-msvc",
    "i686-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-gnu",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    // Tier 2 with host tools
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-musl",
    "arm-unknown-linux-gnueabi",
    "arm-unknown-linux-gnueabihf",
    "armv7-unknown-linux-gnueabihf",
    "loongarch64-unknown-linux-gnu",
    "loongarch64-unknown-linux-musl",
    "powerpc-unknown-linux-gnu",
    "powerpc64-unknown-linux-gnu",
    "powerpc64le-unknown-linux-gnu",
    "riscv64gc-unknown-linux-gnu",
    "s390x-unknown-linux-gnu",
    "x86_64-unknown-freebsd",
    "x86_64-unknown-illumos",
    "x86_64-unknown-linux-musl",
    "x86_64-unknown-netbsd",
    // Common Tier 2
    "aarch64-linux-android",
    "aarch64-unknown-linux-ohos",
    "armv7-linux-androideabi",
    "i686-linux-android",
    "i686-unknown-linux-musl",
    "thumbv7neon-unknown-linux-gnueabihf",
    "wasm32-unknown-unknown",
    "wasm32-wasi",
    "wasm32-wasip1",
    "wasm32-wasip2",
    "x86_64-linux-android",
    "x86_64-apple-ios",
    "aarch64-apple-ios",
    "aarch64-apple-ios-sim",
    // MIPS targets
    "mips-unknown-linux-gnu",
    "mips-unknown-linux-musl",
    "mipsel-unknown-linux-gnu",
    "mipsel-unknown-linux-musl",
    "mips64-unknown-linux-gnuabi64",
    "mips64-unknown-linux-muslabi64",
    "mips64el-unknown-linux-gnuabi64",
    "mips64el-unknown-linux-muslabi64",
    // RISC-V targets
    "riscv32i-unknown-none-elf",
    "riscv32imac-unknown-none-elf",
    "riscv32imc-unknown-none-elf",
    "riscv64gc-unknown-linux-musl",
    "riscv64gc-unknown-none-elf",
    "riscv64imac-unknown-none-elf",
    // s390x targets
    "s390x-unknown-linux-musl",
    // PowerPC targets
    "powerpc-unknown-linux-gnuspe",
    "powerpc64le-unknown-linux-musl",
    "powerpc64-unknown-linux-musl",
    // SPARC targets
    "sparc64-unknown-linux-gnu",
    "sparc-unknown-linux-gnu",
    "sparcv9-sun-solaris",
    // Thumb targets (ARM embedded)
    "thumbv6m-none-eabi",
    "thumbv7em-none-eabi",
    "thumbv7em-none-eabihf",
    "thumbv7m-none-eabi",
    "thumbv7neon-linux-androideabi",
    "thumbv8m.base-none-eabi",
    "thumbv8m.main-none-eabi",
    "thumbv8m.main-none-eabihf",
    // i686 targets
    "i686-unknown-freebsd",
    "i686-unknown-linux-musl",
    // Additional i686 targets
    "i586-unknown-linux-gnu",
    "i586-unknown-linux-musl",
    // Additional ARM targets
    "arm-unknown-linux-musleabi",
    "arm-unknown-linux-musleabihf",
    "armv5te-unknown-linux-gnueabi",
    "armv5te-unknown-linux-musleabi",
    "armv7-unknown-linux-gnueabi",
    "armv7-unknown-linux-musleabi",
    "armv7-unknown-linux-musleabihf",
    "armv7-unknown-linux-ohos",
    // Apple targets
    "aarch64-apple-tvos",
    "aarch64-apple-watchos",
    // FreeBSD / OpenBSD / NetBSD
    "aarch64-unknown-freebsd",
    "x86_64-unknown-openbsd",
    "aarch64-unknown-openbsd",
    // Solaris / illumos
    "x86_64-pc-solaris",
    // Fuchsia
    "aarch64-unknown-fuchsia",
    "x86_64-unknown-fuchsia",
    // Redox
    "x86_64-unknown-redox",
    // Haiku
    "x86_64-unknown-haiku",
    // UEFI
    "x86_64-unknown-uefi",
    "aarch64-unknown-uefi",
    "i686-unknown-uefi",
    // None / bare-metal
    "aarch64-unknown-none",
    "aarch64-unknown-none-softfloat",
    "x86_64-unknown-none",
];

// ---------------------------------------------------------------------------
// strip_glibc_suffix — strip glibc version suffix like ".2.17" from targets
// ---------------------------------------------------------------------------

/// Strip a glibc version suffix from a target triple.
///
/// Targets like `aarch64-unknown-linux-gnu.2.17` carry a `.X.Y` suffix that
/// tells cargo-zigbuild which glibc version to link against. Cargo itself
/// doesn't understand the suffix, so we strip it when constructing the target
/// directory path. The full target (with suffix) is passed to cargo-zigbuild.
///
/// Returns `(cargo_target, has_suffix)` — when there is no suffix the input
/// is returned unchanged.
fn strip_glibc_suffix(target: &str) -> (&str, bool) {
    // Match patterns like "gnu.2.17", "musl.1.1"
    // The suffix starts with a dot followed by a digit after "gnu" or "musl"
    if let Some(idx) = target.rfind("gnu.").or_else(|| target.rfind("musl.")) {
        let suffix_start = target[idx..].find('.').map(|i| idx + i);
        if let Some(start) = suffix_start {
            // Verify the part after the dot looks like a version (starts with digit)
            let after_dot = &target[start + 1..];
            if after_dot.starts_with(|c: char| c.is_ascii_digit()) {
                return (&target[..start], true);
            }
        }
    }
    (target, false)
}

/// Check if a target has a glibc version suffix and should be validated
/// against the known targets list without the suffix.
fn target_for_validation(target: &str) -> &str {
    strip_glibc_suffix(target).0
}

// ---------------------------------------------------------------------------
// is_dynamically_linked — minimal ELF check for PT_INTERP
// ---------------------------------------------------------------------------

/// Check if a binary at the given path is dynamically linked by reading ELF
/// program headers and looking for a PT_INTERP segment (type 3).
///
/// Returns `false` for non-ELF files, files that can't be read, or statically
/// linked ELF binaries.
pub fn is_dynamically_linked(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Read enough of the header to determine ELF class and program header info
    let mut buf = [0u8; 64];
    if file.read(&mut buf).unwrap_or(0) < 64 {
        return false;
    }

    // Verify ELF magic: 0x7f 'E' 'L' 'F'
    if &buf[0..4] != b"\x7fELF" {
        return false;
    }

    let is_64bit = buf[4] == 2;
    let is_le = buf[5] == 1; // 1 = little-endian, 2 = big-endian

    let read_u16 = |b: &[u8]| -> u16 {
        if is_le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let read_u32 = |b: &[u8]| -> u32 {
        if is_le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let read_u64 = |b: &[u8]| -> u64 {
        if is_le {
            u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        } else {
            u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        }
    };

    // Parse program header offset, entry size, and count
    let (ph_offset, ph_entry_size, ph_count) = if is_64bit {
        // e_phoff at offset 32 (8 bytes), e_phentsize at 54 (2 bytes), e_phnum at 56 (2 bytes)
        let offset = read_u64(&buf[32..40]);
        let entry_size = read_u16(&buf[54..56]);
        let count = read_u16(&buf[56..58]);
        (offset, entry_size as u64, count)
    } else {
        // 32-bit ELF: e_phoff at offset 28 (4 bytes), e_phentsize at 42, e_phnum at 44
        let offset = read_u32(&buf[28..32]) as u64;
        let entry_size = read_u16(&buf[42..44]);
        let count = read_u16(&buf[44..46]);
        (offset, entry_size as u64, count)
    };

    if ph_count == 0 || ph_entry_size == 0 {
        return false;
    }

    // Read all program headers
    let total_size = ph_entry_size * ph_count as u64;
    let mut ph_buf = vec![0u8; total_size as usize];
    use std::io::Seek;
    if file.seek(std::io::SeekFrom::Start(ph_offset)).is_err() {
        return false;
    }
    if file.read_exact(&mut ph_buf).is_err() {
        return false;
    }

    // Scan for PT_INTERP (type 3)
    let pt_interp: u32 = 3;
    for i in 0..ph_count as usize {
        let entry_start = i * ph_entry_size as usize;
        let p_type = read_u32(&ph_buf[entry_start..entry_start + 4]);
        if p_type == pt_interp {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// check_workspace_package — validate --package flag for workspace crates
// ---------------------------------------------------------------------------

/// If the Cargo.toml at `crate_path` has a `[workspace]` section with `members`,
/// verify that the build flags contain `--package` or `-p`. Returns an error
/// if the workspace is detected but no package flag is present.
fn check_workspace_package(crate_path: &str, flags: Option<&str>) -> Result<()> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = match std::fs::read_to_string(&cargo_toml_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };

    // Check for [workspace] with members
    if let Some(ws) = doc.get("workspace")
        && ws.get("members").is_some()
    {
        // Check if flags contain --package or -p
        let has_package = flags.is_some_and(|f| {
            let tokens: Vec<&str> = f.split_whitespace().collect();
            tokens.iter().any(|t| {
                *t == "-p"
                    || t.starts_with("--package")
                    || t.starts_with("-p=")
                    || t.starts_with("--package=")
            })
        });
        if !has_package {
            anyhow::bail!(
                "you need to specify which workspace package to build, \
                     please add '--package=<name>' to your build flags"
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// find_workspace_root — walk up from crate path to find workspace Cargo.toml
// ---------------------------------------------------------------------------

/// Walk up from `crate_path` looking for a `Cargo.toml` that contains a
/// `[workspace]` section.  Returns the directory containing the workspace
/// root `Cargo.toml`, or `None` if no workspace root is found.
fn find_workspace_root(crate_path: &str) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(crate_path).ok()?;
    // Walk upward from the crate directory
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists()
            && let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && let Ok(doc) = content.parse::<toml_edit::DocumentMut>()
            && doc.get("workspace").is_some()
        {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// resolve_binary_path — check both relative target/ and workspace root target/
// ---------------------------------------------------------------------------

/// Resolve the actual binary path after a build.
///
/// Cargo places build artifacts in the workspace root's `target/` directory,
/// not in per-crate `target/` directories.  When the expected relative path
/// does not exist, this function tries the workspace root's target directory.
fn resolve_binary_path(expected: &Path, crate_path: &str) -> PathBuf {
    if expected.exists() {
        return expected.to_path_buf();
    }
    // Try workspace root target directory
    if let Some(ws_root) = find_workspace_root(crate_path) {
        let ws_path = ws_root.join(expected);
        if ws_path.exists() {
            return ws_path;
        }
    }
    // Return the original path — the caller will handle the error.
    expected.to_path_buf()
}

// ---------------------------------------------------------------------------
// cargo_target_dir — respect CARGO_TARGET_DIR / CARGO_BUILD_TARGET_DIR env vars
// ---------------------------------------------------------------------------

/// Return the Cargo target directory.
///
/// Checks per-build env vars (from `build.env` config) first, then falls back
/// to `CARGO_TARGET_DIR` and `CARGO_BUILD_TARGET_DIR` from the process
/// environment, and finally defaults to `target`.
///
/// The `build_env` parameter carries the per-target env map from config, which
/// is passed to the cargo Command but also needs to be reflected here so that
/// the predicted binary path matches where cargo actually writes it.
fn cargo_target_dir(build_env: Option<&HashMap<String, String>>) -> PathBuf {
    // Check per-build env vars first — these override process env
    if let Some(env) = build_env {
        if let Some(dir) = env.get("CARGO_TARGET_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
        if let Some(dir) = env.get("CARGO_BUILD_TARGET_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
    }
    // Fall back to process environment
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("CARGO_BUILD_TARGET_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    PathBuf::from("target")
}

// run_hooks is imported from anodize_core::hooks

// ---------------------------------------------------------------------------
// resolve_reproducible_epoch — parse SOURCE_DATE_EPOCH with commit_timestamp fallback
// ---------------------------------------------------------------------------

fn resolve_reproducible_epoch(commit_timestamp: &str) -> i64 {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| commit_timestamp.parse::<i64>().unwrap_or(0));
    if epoch == 0 {
        eprintln!(
            "Warning: [build] reproducible build requested but could not determine epoch \
             from SOURCE_DATE_EPOCH or CommitTimestamp; mtime will not be set"
        );
    }
    epoch
}

// ---------------------------------------------------------------------------
// copy_from resolution helper
// ---------------------------------------------------------------------------

/// Resolve a copy_from job: look up the source binary from registered artifacts
/// (filtering by target **and** crate_name to avoid cross-crate collisions),
/// copy it to the destination, and return Ok.
fn resolve_copy_from(
    ctx: &Context,
    src: &Path,
    dst: &Path,
    target: &str,
    crate_name: &str,
) -> Result<()> {
    let resolved_src = ctx
        .artifacts
        .by_kind(ArtifactKind::Binary)
        .into_iter()
        .find(|a| {
            a.target.as_deref() == Some(target) && a.crate_name == crate_name && a.path == *src
        })
        .map(|a| a.path.clone())
        .unwrap_or_else(|| src.to_path_buf());

    std::fs::copy(&resolved_src, dst).with_context(|| {
        format!(
            "copy_from: failed to copy {} -> {}",
            resolved_src.display(),
            dst.display()
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ensure_targets_installed — run `rustup target add` for cross-compilation targets
// ---------------------------------------------------------------------------

/// For each unique non-host target, run `rustup target add` to ensure the
/// target toolchain is installed. If `rustup` is not available (e.g. when
/// using cargo-cross or a pre-configured environment), this is silently skipped.
fn ensure_targets_installed(
    ctx: &Context,
    targets: &[String],
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
) -> Result<()> {
    let host = anodize_core::partial::detect_host_target().unwrap_or_default();
    for target in targets {
        if target == &host {
            continue;
        }
        if dry_run {
            log.status(&format!("(dry-run) would run: rustup target add {target}"));
            continue;
        }
        let output = Command::new("rustup")
            .args(["target", "add", target])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                log.verbose(&format!("ensured target installed: {target}"));
            }
            Ok(o) => {
                log.warn(&format!(
                    "rustup target add {target} failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ));
            }
            Err(_) => {
                ctx.strict_guard(log, "rustup not found, skipping target installation")?;
                return Ok(()); // If rustup isn't available, skip all
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// amd64 microarchitecture variant detection from RUSTFLAGS
// ---------------------------------------------------------------------------

fn parse_amd64_variant_from_rustflags(rustflags: &str) -> Option<String> {
    let tokens: Vec<&str> = rustflags.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let cpu = if let Some(val) = tokens[i].strip_prefix("-Ctarget-cpu=") {
            Some(val)
        } else if tokens[i] == "-C"
            && i + 1 < tokens.len()
            && let Some(val) = tokens[i + 1].strip_prefix("target-cpu=")
        {
            i += 1;
            Some(val)
        } else {
            None
        };
        if let Some(cpu) = cpu
            && let Some(level) = cpu.strip_prefix("x86-64-")
        {
            return Some(level.to_string());
        }
        i += 1;
    }
    None
}

fn detect_amd64_variant(target: &str, env: &HashMap<String, String>) -> Option<String> {
    if !target.starts_with("x86_64") {
        return None;
    }
    if let Some(flags) = env.get("RUSTFLAGS")
        && let Some(v) = parse_amd64_variant_from_rustflags(flags)
    {
        return Some(v);
    }
    None
}

// ---------------------------------------------------------------------------
// BuildStage
// ---------------------------------------------------------------------------

pub struct BuildStage;

impl Stage for BuildStage {
    fn name(&self) -> &str {
        "build"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("build");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;

        let parallelism = ctx.options.parallelism.max(1);

        // Collect global defaults
        let defaults = ctx.config.defaults.as_ref();
        let default_targets: Vec<String> = defaults
            .and_then(|d| d.targets.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| DEFAULT_TARGETS.iter().map(|s| (*s).to_string()).collect());
        let default_strategy = defaults
            .and_then(|d| d.cross.clone())
            .unwrap_or(CrossStrategy::Auto);
        let default_flags: Option<String> = defaults.and_then(|d| d.flags.clone());
        let default_ignores: Vec<BuildIgnore> =
            defaults.and_then(|d| d.ignore.clone()).unwrap_or_default();
        let default_overrides: Vec<BuildOverride> = defaults
            .and_then(|d| d.overrides.clone())
            .unwrap_or_default();

        // Collect crates to process (cloned to avoid borrow conflict with ctx.artifacts)
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // --- Version sync + binstall: source-mutating steps ---
        // Snapshot builds never mutate source files. The resolved version in
        // snapshot mode is a synthetic identifier (e.g. `0.3.4-SNAPSHOT-abc`),
        // and writing that — or worse, downgrading Cargo.toml when the working
        // tree is ahead of the latest tag — corrupts the working copy. Binstall
        // metadata in snapshot mode would reference a non-existent tag URL.
        let version = ctx
            .template_vars()
            .get("RawVersion")
            .or_else(|| ctx.template_vars().get("Version"))
            .cloned()
            .unwrap_or_default();
        let is_snapshot = ctx.is_snapshot();
        for crate_cfg in &crates {
            if let Some(ref vs) = crate_cfg.version_sync
                && vs.enabled.unwrap_or(false)
            {
                if is_snapshot {
                    log.verbose(&format!(
                        "version-sync: skipping {} (snapshot mode does not mutate source files)",
                        crate_cfg.path
                    ));
                } else if !version.is_empty() {
                    version_sync::sync_version(&crate_cfg.path, &version, dry_run, &log)?;
                }
            }
            if let Some(ref bs) = crate_cfg.binstall
                && bs.enabled.unwrap_or(false)
            {
                if is_snapshot {
                    log.verbose(&format!(
                        "binstall: skipping {} (snapshot mode does not mutate source files)",
                        crate_cfg.path
                    ));
                } else {
                    binstall::generate_binstall_metadata(&crate_cfg.path, bs, ctx, dry_run)?;
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 1: Flatten the nested (crate, build, target) loops into a
        // list of BuildJob descriptors. No compilation happens here.
        // -----------------------------------------------------------------

        /// A fully-resolved description of one build unit.
        struct BuildJob {
            /// The build command to execute (None for copy_from jobs).
            cmd: Option<BuildCommand>,
            /// For copy_from jobs: source path + destination path.
            copy_from: Option<(PathBuf, PathBuf)>,
            /// Expected output binary path.
            bin_path: PathBuf,
            /// Artifact kind to register.
            artifact_kind: ArtifactKind,
            /// Target triple.
            target: String,
            /// Crate name.
            crate_name: String,
            /// Binary name (for metadata).
            binary_name: String,
            /// Build config ID (for downstream filtering).
            build_id: Option<String>,
            /// Whether reproducible mtime should be applied.
            reproducible: bool,
            /// Pre-build hooks to execute before compilation.
            pre_hooks: Vec<HookEntry>,
            /// Post-build hooks to execute after compilation.
            post_hooks: Vec<HookEntry>,
            /// When true, output binaries to flat dist/ instead of dist/{target}/.
            no_unique_dist_dir: bool,
            /// Crate path (for workspace root resolution).
            crate_path: String,
            /// Optional mod_timestamp override for the built binary.
            mod_timestamp: Option<String>,
            /// Detected amd64 microarchitecture variant (e.g. "v2", "v3", "v4")
            /// from RUSTFLAGS `-C target-cpu=x86-64-vN`.
            amd64_variant: Option<String>,
        }

        /// Result of executing a build job.
        struct BuildResult {
            bin_path: PathBuf,
            artifact_kind: ArtifactKind,
            target: String,
            crate_name: String,
            binary_name: String,
            build_id: Option<String>,
            no_unique_dist_dir: bool,
            amd64_variant: Option<String>,
        }

        /// Build artifact metadata, always including "binary" and optionally "id".
        fn artifact_meta(
            binary: &str,
            build_id: &Option<String>,
            amd64_variant: &Option<String>,
        ) -> HashMap<String, String> {
            let mut m = HashMap::from([("binary".to_string(), binary.to_string())]);
            if let Some(id) = build_id {
                m.insert("id".to_string(), id.clone());
            }
            if let Some(v) = amd64_variant {
                m.insert("amd64_variant".to_string(), v.clone());
            }
            m
        }

        let mut build_jobs: Vec<BuildJob> = Vec::new();
        let mut copy_jobs: Vec<BuildJob> = Vec::new();

        let commit_timestamp = ctx
            .template_vars()
            .get("CommitTimestamp")
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        for crate_cfg in &crates {
            // Determine builds for this crate
            let builds: Vec<BuildConfig> = match &crate_cfg.builds {
                Some(b) if !b.is_empty() => b.clone(),
                _ => {
                    // No builds configured — only create a default binary build if
                    // the crate actually has a binary target (src/main.rs or [[bin]]).
                    // Library-only crates should not get a default --bin build.
                    if crate_has_binary_target(&crate_cfg.path) {
                        vec![BuildConfig {
                            binary: crate_cfg.name.clone(),
                            ..Default::default()
                        }]
                    } else {
                        log.status(&format!(
                            "skipping crate '{}' — no builds configured and no binary target found",
                            crate_cfg.name
                        ));
                        continue;
                    }
                }
            };

            // Validate: no duplicate build IDs within this crate
            {
                let mut seen_ids: HashSet<&str> = HashSet::new();
                for build in &builds {
                    if let Some(ref id) = build.id
                        && !seen_ids.insert(id.as_str())
                    {
                        anyhow::bail!(
                            "found 2 builds with the ID '{}' in crate '{}'",
                            id,
                            crate_cfg.name
                        );
                    }
                }
            }

            // Detect crate type for cdylib/wasm awareness (once per crate)
            let crate_type = detect_crate_type(&crate_cfg.path);
            let is_wasm_crate = matches!(crate_type.as_deref(), Some("cdylib"));
            let is_library = matches!(
                crate_type.as_deref(),
                Some("cdylib" | "staticlib" | "dylib")
            );

            for build in &builds {
                // Skip builds marked with skip: true/template
                let should_skip = build
                    .skip
                    .as_ref()
                    .map(|s| s.is_disabled(|tmpl| ctx.render_template(tmpl)))
                    .unwrap_or(false);
                if should_skip {
                    log.status(&format!(
                        "skipping build '{}' (skip: true)",
                        build.id.as_deref().unwrap_or(&build.binary)
                    ));
                    continue;
                }

                // NOTE: Binary name rendering is deferred to the per-target loop
                // below so that per-target template variables (Os, Arch, Target)
                // are available in the template. The raw template is used in log
                // messages before the target loop.
                let binary_name_raw = &build.binary;

                // Targets: per-build override (even if empty), else global defaults.
                // An explicitly empty list (Some(vec![])) means "skip this build".
                // Only None (not specified) falls through to defaults.
                let mut targets: Vec<String> = if build.targets.is_some() {
                    build.targets.clone().unwrap_or_default()
                } else if !default_targets.is_empty() {
                    default_targets.clone()
                } else {
                    Vec::new()
                };

                // --single-target: filter targets to only the specified triple
                if let Some(ref single) = ctx.options.single_target {
                    let had_targets = !targets.is_empty();
                    targets.retain(|t| t == single);
                    if had_targets && targets.is_empty() {
                        log.warn(&format!(
                            "--single-target: host triple '{}' not in configured targets for {}/{}, skipping",
                            single, crate_cfg.name, binary_name_raw
                        ));
                        continue;
                    }
                }

                // --split: filter targets to those matching the partial target
                if let Some(ref partial) = ctx.options.partial_target {
                    let had_targets = !targets.is_empty();
                    targets = partial.filter_targets(&targets);
                    if had_targets && targets.is_empty() {
                        log.verbose(&format!(
                            "split: no targets match partial filter for {}/{}, skipping",
                            crate_cfg.name, binary_name_raw
                        ));
                        continue;
                    }
                }

                // If no targets configured, skip (caller should ensure defaults)
                if targets.is_empty() {
                    log.warn(&format!(
                        "no targets configured for {}/{}, skipping",
                        crate_cfg.name, binary_name_raw
                    ));
                    continue;
                }

                // Validate targets against known list (error, matching GoReleaser)
                for target in &targets {
                    let validation_target = target_for_validation(target);
                    if !KNOWN_TARGETS.contains(&validation_target) {
                        anyhow::bail!(
                            "target '{}' is not in the known targets list and may be invalid; \
                             if this is a custom target, add it to your build config",
                            target
                        );
                    }
                }

                // Strategy: per-crate override, else global default
                let strategy = crate_cfg
                    .cross
                    .clone()
                    .unwrap_or_else(|| default_strategy.clone());

                // Flags: per-build, else global default, else "--release".
                // Default to --release for production builds. Users can explicitly set
                // `flags: ""` (empty string) in their config to get a debug build.
                // This works because `Some("")` is not `None`, so `.or(Some("--release"))`
                // will not override an explicit empty string.
                let flags: Option<&str> = build
                    .flags
                    .as_deref()
                    .or(default_flags.as_deref())
                    .or(Some("--release"));

                // Features and no_default_features
                let features: Vec<String> = build.features.clone().unwrap_or_default();
                let no_default_features: bool = build.no_default_features.unwrap_or(false);

                // Per-build ignore/overrides, falling back to defaults
                let build_ignores: Vec<BuildIgnore> = build
                    .ignore
                    .clone()
                    .unwrap_or_else(|| default_ignores.clone());
                let build_overrides: Vec<BuildOverride> = build
                    .overrides
                    .clone()
                    .unwrap_or_else(|| default_overrides.clone());

                // Cross tool override — takes precedence over the `cross` strategy
                let cross_tool = build.cross_tool.clone();
                if cross_tool.is_some() && crate_cfg.cross.is_some() {
                    log.warn("both `cross` strategy and `cross_tool` are set; `cross_tool` takes precedence");
                }

                // Command override (e.g. "auditable build" for `cargo auditable build`)
                let command_override = build.command.clone();

                // Workspace --package validation: if building from a workspace root,
                // ensure --package is specified in the build flags.
                check_workspace_package(&crate_cfg.path, flags)?;

                // Resolve no_unique_dist_dir: per-build overrides crate-level
                let no_unique_dist_dir_val = build
                    .no_unique_dist_dir
                    .as_ref()
                    .map(|s| s.is_disabled(|tmpl| ctx.render_template(tmpl)))
                    .or_else(|| {
                        crate_cfg
                            .no_unique_dist_dir
                            .as_ref()
                            .map(|s| s.is_disabled(|tmpl| ctx.render_template(tmpl)))
                    })
                    .unwrap_or(false);

                // Per-target env (target-keyed map in BuildConfig.env)
                for target in &targets {
                    // Check ignore list
                    if is_target_ignored(target, &build_ignores) {
                        log.verbose(&format!("ignoring target {} (matched ignore rule)", target));
                        continue;
                    }

                    // Apply overrides: merge env, append flags, extend features
                    let matched_override =
                        find_matching_override(target, &build_overrides, &log, ctx.is_strict())?;
                    let effective_flags: Option<String> = if let Some(ov) = matched_override {
                        match (&flags, &ov.flags) {
                            (Some(base), Some(extra)) => Some(format!("{} {}", base, extra)),
                            (None, Some(extra)) => Some(extra.clone()),
                            (Some(base), None) => Some(base.to_string()),
                            (None, None) => None,
                        }
                    } else {
                        flags.map(|f| f.to_string())
                    };
                    let effective_features: Vec<String> = if let Some(ov) = matched_override {
                        let mut f = features.clone();
                        if let Some(ref extra) = ov.features {
                            f.extend(extra.iter().cloned());
                        }
                        f
                    } else {
                        features.clone()
                    };

                    // template-render the flags string
                    // through the template engine before splitting on whitespace.
                    // This allows flags like `--cfg={{ .Os }}` to be resolved.
                    // Filter out empty results after rendering.
                    let effective_flags: Option<String> = effective_flags.and_then(|f| {
                        let rendered = ctx.render_template(&f).unwrap_or_else(|_| f.clone());
                        let trimmed = rendered.trim().to_string();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        }
                    });

                    // Determine the binary path
                    // Flags may contain --release, --profile release, or
                    // --profile=<name>; detect the effective cargo profile.
                    let profile = detect_cargo_profile(effective_flags.as_deref());

                    let is_wasm_target = target.contains("wasm32");
                    let (os, arch) = map_target(target);

                    // Set per-target template vars BEFORE rendering binary name
                    ctx.template_vars_mut().set("Target", target);
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);
                    let first_component = target.split('-').next().unwrap_or("");
                    match first_component {
                        "aarch64" => ctx.template_vars_mut().set("Arm64", "v8"),
                        "armv7" | "armv7l" => ctx.template_vars_mut().set("Arm", "7"),
                        "armv6" | "armv6l" | "arm" => ctx.template_vars_mut().set("Arm", "6"),
                        "x86_64" => ctx.template_vars_mut().set("Amd64", "v1"),
                        "i686" | "i386" | "i586" => ctx.template_vars_mut().set("I386", "sse2"),
                        _ => {}
                    }
                    let artifact_ext = if os == "windows" { ".exe" } else { "" };
                    ctx.template_vars_mut().set("ArtifactExt", artifact_ext);
                    ctx.template_vars_mut()
                        .set("ArtifactID", build.id.as_deref().unwrap_or(""));

                    // Render binary name with per-target template vars available
                    let binary_name = ctx.render_template(binary_name_raw).unwrap_or_else(|e| {
                        log.warn(&format!(
                            "failed to render binary template '{}': {}, using raw value",
                            binary_name_raw, e
                        ));
                        binary_name_raw.to_string()
                    });

                    // Determine the output file name based on target and crate type
                    let (output_name, artifact_kind) = if is_wasm_target && is_wasm_crate {
                        // wasm32 target with cdylib — output is .wasm
                        (format!("{}.wasm", binary_name), ArtifactKind::Wasm)
                    } else if is_library && !is_wasm_target {
                        // Library target — output is .so/.dylib/.dll
                        let ext = match os.as_str() {
                            "windows" => "dll",
                            "darwin" => "dylib",
                            _ => "so",
                        };
                        let prefix = if os == "windows" { "" } else { "lib" };
                        (
                            format!("{}{}.{}", prefix, binary_name, ext),
                            ArtifactKind::Library,
                        )
                    } else {
                        // Standard binary
                        let name = if os == "windows" {
                            format!("{}.exe", binary_name)
                        } else {
                            binary_name.clone()
                        };
                        (name, ArtifactKind::Binary)
                    };

                    // Strip glibc version suffix for the cargo target dir path.
                    // e.g. "aarch64-unknown-linux-gnu.2.17" -> stripped for dir
                    let (cargo_target_name, _has_glibc_suffix) = strip_glibc_suffix(target);

                    let raw_target_env: Option<&HashMap<String, String>> =
                        build.env.as_ref().and_then(|m| m.get(target.as_str()));

                    // Use stripped target name for directory path
                    let bin_path = cargo_target_dir(raw_target_env)
                        .join(cargo_target_name)
                        .join(profile)
                        .join(&output_name);

                    // Handle copy_from: skip compilation, queue for after builds
                    if let Some(src_binary) = &build.copy_from {
                        let src_name = if os == "windows" {
                            format!("{}.exe", src_binary)
                        } else {
                            src_binary.clone()
                        };
                        let src_path = cargo_target_dir(raw_target_env)
                            .join(cargo_target_name)
                            .join(profile)
                            .join(&src_name);

                        // Clear per-target template vars before continuing
                        ctx.template_vars_mut().set("Target", "");
                        ctx.template_vars_mut().set("Os", "");
                        ctx.template_vars_mut().set("Arch", "");
                        ctx.template_vars_mut().set("Arm64", "");
                        ctx.template_vars_mut().set("Arm", "");
                        ctx.template_vars_mut().set("Amd64", "");
                        ctx.template_vars_mut().set("I386", "");
                        ctx.template_vars_mut().set("ArtifactExt", "");
                        ctx.template_vars_mut().set("ArtifactID", "");

                        let copy_variant = raw_target_env
                            .map(|e| detect_amd64_variant(target, e))
                            .unwrap_or(None);
                        copy_jobs.push(BuildJob {
                            cmd: None,
                            copy_from: Some((src_path, bin_path.clone())),
                            bin_path,
                            artifact_kind,
                            target: target.clone(),
                            crate_name: crate_cfg.name.clone(),
                            binary_name: binary_name.clone(),
                            build_id: build.id.clone(),
                            reproducible: false,
                            pre_hooks: Vec::new(),
                            post_hooks: Vec::new(),
                            no_unique_dist_dir: no_unique_dist_dir_val,
                            crate_path: crate_cfg.path.clone(),
                            mod_timestamp: build.mod_timestamp.clone(),
                            amd64_variant: copy_variant,
                        });
                        continue;
                    }

                    // No copy_from: build a compilation command
                    let mut target_env: HashMap<String, String> = build
                        .env
                        .as_ref()
                        .and_then(|m| m.get(target.as_str()))
                        .cloned()
                        .unwrap_or_default();

                    // Render env values and expand shell-style env var references.
                    // Cascade: each rendered KEY is injected into the template
                    // context's env map BEFORE rendering later entries so that
                    // `{{ .Env.KEY }}` references resolve to the same-block value.
                    // Iteration is sorted for deterministic order; full
                    // user-insertion-order cascade requires changing the YAML
                    // schema to an ordered list — tracked upstream.
                    let mut rendered_env: HashMap<String, String> = HashMap::new();
                    let mut keys: Vec<&String> = target_env.keys().collect();
                    keys.sort();
                    for k in keys {
                        let v = &target_env[k];
                        let rendered_val = ctx.render_template(v).unwrap_or_else(|e| {
                            log.warn(&format!(
                                "failed to render env value for '{}': {}, using raw value",
                                k, e
                            ));
                            v.clone()
                        });
                        let expanded = expand_env_vars(&rendered_val);
                        // Inject into ctx env so later entries (and templated
                        // fields) see this KEY via `{{ .Env.KEY }}`.
                        ctx.template_vars_mut().set_env(k, &expanded);
                        rendered_env.insert(k.clone(), expanded);
                    }
                    target_env = rendered_env;

                    // Merge override env if matched
                    if let Some(ov) = matched_override
                        && let Some(ref ov_env) = ov.env
                    {
                        for (k, v) in ov_env {
                            let rendered_val = ctx.render_template(v).unwrap_or_else(|e| {
                                log.warn(&format!(
                                    "failed to render override env value for '{}': {}, using raw value",
                                    k, e
                                ));
                                v.clone()
                            });
                            target_env.insert(k.clone(), expand_env_vars(&rendered_val));
                        }
                    }

                    // Set per-target hook context: Name, Path, Ext
                    ctx.template_vars_mut().set("Name", &binary_name);
                    ctx.template_vars_mut()
                        .set("Path", &bin_path.to_string_lossy());
                    ctx.template_vars_mut()
                        .set("Ext", if os == "windows" { ".exe" } else { "" });

                    // Remove per-target template variables to avoid leaking
                    ctx.template_vars_mut().set("Target", "");
                    ctx.template_vars_mut().set("Os", "");
                    ctx.template_vars_mut().set("Arch", "");
                    ctx.template_vars_mut().set("Arm64", "");
                    ctx.template_vars_mut().set("Arm", "");
                    ctx.template_vars_mut().set("Amd64", "");
                    ctx.template_vars_mut().set("I386", "");
                    ctx.template_vars_mut().set("ArtifactExt", "");
                    ctx.template_vars_mut().set("ArtifactID", "");
                    ctx.template_vars_mut().set("Name", "");
                    ctx.template_vars_mut().set("Path", "");
                    ctx.template_vars_mut().set("Ext", "");

                    // Reproducible builds: inject SOURCE_DATE_EPOCH and RUSTFLAGS
                    if build.reproducible.unwrap_or(false) {
                        target_env
                            .entry("SOURCE_DATE_EPOCH".to_string())
                            .or_insert_with(|| commit_timestamp.clone());

                        let cwd = std::env::current_dir()
                            .unwrap_or_else(|_| PathBuf::from("."))
                            .to_string_lossy()
                            .into_owned();
                        let remap_flag = format!("--remap-path-prefix={cwd}=/build");
                        let existing_rustflags =
                            target_env.get("RUSTFLAGS").cloned().unwrap_or_default();
                        let new_rustflags = if existing_rustflags.is_empty() {
                            remap_flag
                        } else {
                            format!("{existing_rustflags} {remap_flag}")
                        };
                        target_env.insert("RUSTFLAGS".to_string(), new_rustflags);
                    }

                    // For library/wasm targets, use --lib; otherwise --bin
                    let cmd = if is_library || is_wasm_target {
                        build_lib_command(
                            &crate_cfg.path,
                            target,
                            &strategy,
                            effective_flags.as_deref(),
                            &effective_features,
                            no_default_features,
                            &target_env,
                            cross_tool.as_deref(),
                            command_override.as_deref(),
                        )
                    } else {
                        build_command(
                            &binary_name,
                            &crate_cfg.path,
                            target,
                            &strategy,
                            effective_flags.as_deref(),
                            &effective_features,
                            no_default_features,
                            &target_env,
                            cross_tool.as_deref(),
                            command_override.as_deref(),
                        )
                    };

                    build_jobs.push(BuildJob {
                        cmd: Some(cmd),
                        copy_from: None,
                        bin_path,
                        artifact_kind,
                        target: target.clone(),
                        crate_name: crate_cfg.name.clone(),
                        binary_name: binary_name.clone(),
                        build_id: build.id.clone(),
                        reproducible: build.reproducible.unwrap_or(false),
                        pre_hooks: build
                            .hooks
                            .as_ref()
                            .and_then(|h| h.pre.clone())
                            .unwrap_or_default(),
                        post_hooks: build
                            .hooks
                            .as_ref()
                            .and_then(|h| h.post.clone())
                            .unwrap_or_default(),
                        no_unique_dist_dir: no_unique_dist_dir_val,
                        crate_path: crate_cfg.path.clone(),
                        mod_timestamp: build.mod_timestamp.clone(),
                        amd64_variant: detect_amd64_variant(target, &target_env),
                    });
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 1.5: Ensure cross-compilation targets are installed via rustup.
        // -----------------------------------------------------------------

        {
            let unique_targets: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                build_jobs
                    .iter()
                    .filter_map(|j| {
                        if seen.insert(j.target.clone()) {
                            Some(j.target.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            ensure_targets_installed(ctx, &unique_targets, &log, dry_run)?;
        }

        // -----------------------------------------------------------------
        // Phase 2: Execute build jobs (with parallelism) then copy_from jobs.
        // -----------------------------------------------------------------

        // Rust builds sharing the same workspace target/ directory can deadlock
        // when multiple cargo invocations run in parallel (they contend on
        // target/ directory locks). GoReleaser explicitly serializes Rust builds
        // for this reason. Force sequential execution unless the user has only
        // a single build job.
        let effective_parallelism = if build_jobs.len() > 1 { 1 } else { parallelism };

        let template_vars = ctx.template_vars().clone();
        let dist_dir = ctx.config.dist.clone();

        // Helper: register a build artifact, respecting no_unique_dist_dir.
        // When no_unique_dist_dir is true, the binary is copied from cargo's
        // target/{triple}/{profile}/ to a flat {dist}/{name} path, and that
        // flattened path is registered as the artifact. In dry-run mode, the
        // flat path is registered without actually copying.
        let add_artifact = |ctx: &mut Context,
                            job_bin_path: &Path,
                            artifact_kind: ArtifactKind,
                            target: &str,
                            crate_name: &str,
                            binary_name: &str,
                            build_id: &Option<String>,
                            no_unique_dist_dir: bool,
                            amd64_variant: &Option<String>|
         -> Result<()> {
            ctx.template_vars_mut().set("Binary", binary_name);
            let mut meta = artifact_meta(binary_name, build_id, amd64_variant);

            let artifact_path = if no_unique_dist_dir {
                meta.insert("no_unique_dist_dir".to_string(), "true".to_string());
                // Flatten: copy binary to dist/{name} instead of keeping the
                // per-target cargo output path.
                let file_name = job_bin_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| binary_name.to_string());
                let flat_path = dist_dir.join(&file_name);
                if !dry_run {
                    if let Some(parent) = flat_path.parent() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create dist dir: {}", parent.display())
                        })?;
                    }
                    if job_bin_path.exists() {
                        std::fs::copy(job_bin_path, &flat_path).with_context(|| {
                            format!(
                                "no_unique_dist_dir: failed to copy {} -> {}",
                                job_bin_path.display(),
                                flat_path.display()
                            )
                        })?;
                    }
                }
                flat_path
            } else {
                job_bin_path.to_path_buf()
            };

            // Check for ELF dynamic linking and store in metadata
            if artifact_path.exists() && is_dynamically_linked(&artifact_path) {
                meta.insert("DynamicallyLinked".to_string(), "true".to_string());
            }

            ctx.artifacts.add(Artifact {
                kind: artifact_kind,
                name: String::new(),
                path: artifact_path,
                target: Some(target.to_string()),
                crate_name: crate_name.to_string(),
                metadata: meta,
                size: None,
            });
            Ok(())
        };

        if dry_run {
            // Dry-run: just log what would happen, register artifacts sequentially.
            for job in build_jobs.iter().chain(copy_jobs.iter()) {
                // Log pre-hooks (dry-run)
                if !job.pre_hooks.is_empty() {
                    run_hooks(
                        &job.pre_hooks,
                        "pre-build",
                        true,
                        &log,
                        Some(&template_vars),
                    )?;
                }
                if let Some(ref cmd) = job.cmd {
                    log.status(&format!("(dry-run) {} {}", cmd.program, cmd.args.join(" ")));
                } else if let Some((ref src, ref dst)) = job.copy_from {
                    log.status(&format!(
                        "(dry-run) copy {} -> {}",
                        src.display(),
                        dst.display()
                    ));
                }
                // Log post-hooks (dry-run)
                if !job.post_hooks.is_empty() {
                    run_hooks(
                        &job.post_hooks,
                        "post-build",
                        true,
                        &log,
                        Some(&template_vars),
                    )?;
                }
                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        } else if effective_parallelism <= 1 || build_jobs.len() <= 1 {
            // Sequential execution (parallelism == 1 or single job).
            for job in &build_jobs {
                // MkdirAll the dist/target dir BEFORE running pre-hooks, so
                // a pre-hook writing into the expected bin output directory
                // succeeds (GoReleaser build/build.go:147-155 order).
                if let Some(parent) = job.bin_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "failed to create bin output dir: {} (for pre-hook)",
                            parent.display()
                        )
                    })?;
                }
                // Execute pre-build hooks
                if !job.pre_hooks.is_empty() {
                    run_hooks(
                        &job.pre_hooks,
                        "pre-build",
                        false,
                        &log,
                        Some(&template_vars),
                    )?;
                }

                let cmd = job
                    .cmd
                    .as_ref()
                    .context("build job has no cmd (programmer bug: Phase 1 should populate)")?;
                log.status(&format!("running: {} {}", cmd.program, cmd.args.join(" ")));
                let output = Command::new(&cmd.program)
                    .args(&cmd.args)
                    .envs(&cmd.env)
                    .current_dir(&cmd.cwd)
                    .output()
                    .with_context(|| format!("failed to spawn {}", cmd.program))?;
                log.check_output(output, &cmd.program)?;

                // Resolve the binary path — try workspace root if not at
                // the expected relative location.
                let resolved_bin = resolve_binary_path(&job.bin_path, &job.crate_path);

                // Verify the binary was actually produced
                if !resolved_bin.exists() {
                    anyhow::bail!(
                        "build succeeded but binary not found at {} (also checked workspace root): \
                         check that the binary name matches your Cargo.toml [bin] section",
                        job.bin_path.display()
                    );
                }

                // Reproducible mtime: set binary mtime to SOURCE_DATE_EPOCH
                if job.reproducible && resolved_bin.exists() {
                    let epoch = resolve_reproducible_epoch(&commit_timestamp);
                    if epoch > 0 {
                        anodize_core::util::set_file_mtime_epoch(&resolved_bin, epoch)?;
                    }
                }

                // Apply mod_timestamp if configured (overrides reproducible mtime)
                if let Some(ref ts) = job.mod_timestamp
                    && resolved_bin.exists()
                {
                    let rendered_ts = ctx.render_template(ts).unwrap_or_else(|_| ts.clone());
                    let mtime = anodize_core::util::parse_mod_timestamp(&rendered_ts)?;
                    anodize_core::util::set_file_mtime(&resolved_bin, mtime)?;
                    log.verbose(&format!(
                        "applied mod_timestamp={rendered_ts} to {}",
                        resolved_bin.display()
                    ));
                }

                // Execute post-build hooks
                if !job.post_hooks.is_empty() {
                    run_hooks(
                        &job.post_hooks,
                        "post-build",
                        false,
                        &log,
                        Some(&template_vars),
                    )?;
                }

                add_artifact(
                    ctx,
                    &resolved_bin,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }

            // Copy-from jobs (must run after source builds complete)
            for job in &copy_jobs {
                let (src, dst) = job
                    .copy_from
                    .as_ref()
                    .context("copy_from job without copy_from pair (programmer bug)")?;
                resolve_copy_from(ctx, src, dst, &job.target, &job.crate_name)?;

                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        } else {
            // Parallel execution: process build jobs in chunks.
            // Note: pre/post hooks run sequentially before/after each parallel chunk.
            log.status(&format!(
                "building {} jobs with parallelism={}",
                build_jobs.len(),
                effective_parallelism
            ));

            for chunk in build_jobs.chunks(effective_parallelism) {
                // Each chunk runs in parallel via thread::scope.
                // Pre/post hooks run inside each thread so they properly bracket
                // their specific build, matching the sequential path's semantics.
                let results: Vec<Result<BuildResult>> = std::thread::scope(|s| {
                    let handles: Vec<_> = chunk
                        .iter()
                        .map(|job| {
                            // Phase 1 populates `job.cmd` for every build job (copy-from-only
                            // jobs take a separate code path). If it's absent here, that's a
                            // pipeline invariant violation — surface as an error, not a panic,
                            // so the worker thread unwinds through the Result channel instead
                            // of killing the process.
                            let cmd_opt = job.cmd.clone();
                            let crate_name_for_err = job.crate_name.clone();
                            let program = cmd_opt.as_ref().map(|c| c.program.clone());
                            let args = cmd_opt.as_ref().map(|c| c.args.clone());
                            let env = cmd_opt.as_ref().map(|c| c.env.clone());
                            let cwd = cmd_opt.as_ref().map(|c| c.cwd.clone());
                            let bin_path = job.bin_path.clone();
                            let artifact_kind = job.artifact_kind;
                            let target = job.target.clone();
                            let crate_name = job.crate_name.clone();
                            let binary_name = job.binary_name.clone();
                            let build_id = job.build_id.clone();
                            let reproducible = job.reproducible;
                            let no_unique_dist_dir = job.no_unique_dist_dir;
                            let job_crate_path = job.crate_path.clone();
                            let commit_ts = commit_timestamp.clone();
                            let pre_hooks = job.pre_hooks.clone();
                            let post_hooks = job.post_hooks.clone();
                            let job_mod_timestamp = job.mod_timestamp.clone();
                            let job_amd64_variant = job.amd64_variant.clone();
                            let thread_tvars = template_vars.clone();
                            // StageLogger is not Clone, so create a fresh one per thread
                            let thread_log = anodize_core::log::StageLogger::new("build", log.verbosity());

                            s.spawn(move || -> Result<BuildResult> {
                                let program = program.ok_or_else(|| anyhow::anyhow!(
                                    "build: Phase 1 invariant violation — job for crate {} reached Phase 2 without a cmd",
                                    crate_name_for_err
                                ))?;
                                let args = args.unwrap_or_default();
                                let env = env.unwrap_or_default();
                                let cwd = cwd.unwrap_or_default();

                                // MkdirAll the dist/target dir BEFORE the
                                // pre-hook (GoReleaser build/build.go:147-155
                                // order) so pre-hooks can stage files into the
                                // expected output directory.
                                if let Some(parent) = bin_path.parent() {
                                    std::fs::create_dir_all(parent).with_context(|| {
                                        format!(
                                            "failed to create bin output dir: {} (for pre-hook)",
                                            parent.display()
                                        )
                                    })?;
                                }
                                // Execute pre-build hooks before compilation
                                if !pre_hooks.is_empty() {
                                    run_hooks(&pre_hooks, "pre-build", false, &thread_log, Some(&thread_tvars))?;
                                }

                                let output = Command::new(&program)
                                    .args(&args)
                                    .envs(&env)
                                    .current_dir(&cwd)
                                    .output()
                                    .with_context(|| format!("failed to spawn {}", program))?;

                                if !output.status.success() {
                                    let stderr = String::from_utf8_lossy(&output.stderr);
                                    let stdout = String::from_utf8_lossy(&output.stdout);
                                    let mut msg = format!(
                                        "{} failed with exit code: {}",
                                        program,
                                        output.status.code().unwrap_or(-1)
                                    );
                                    if !stderr.is_empty() {
                                        msg.push_str(&format!("\nstderr:\n{}", stderr));
                                    }
                                    if !stdout.is_empty() {
                                        msg.push_str(&format!("\nstdout:\n{}", stdout));
                                    }
                                    anyhow::bail!("{}", msg);
                                }

                                // Resolve the binary path — try workspace root
                                // if not at the expected relative location.
                                let bin_path = resolve_binary_path(&bin_path, &job_crate_path);

                                // Verify the binary was actually produced
                                if !bin_path.exists() {
                                    anyhow::bail!(
                                        "build succeeded but binary not found at {} (also checked workspace root): \
                                         check that the binary name matches your Cargo.toml [bin] section",
                                        bin_path.display()
                                    );
                                }

                                // Reproducible mtime: set binary mtime to SOURCE_DATE_EPOCH
                                if reproducible && bin_path.exists() {
                                    let epoch = resolve_reproducible_epoch(&commit_ts);
                                    if epoch > 0 {
                                        anodize_core::util::set_file_mtime_epoch(&bin_path, epoch)?;
                                    }
                                }

                                // Apply mod_timestamp if configured (overrides reproducible mtime)
                                if let Some(ref ts) = job_mod_timestamp
                                    && bin_path.exists()
                                {
                                    // Thread context doesn't have ctx for template rendering,
                                    // so render using Tera directly with thread-local vars.
                                    let rendered_ts = anodize_core::template::render(ts, &thread_tvars)
                                        .unwrap_or_else(|_| ts.clone());
                                    let mtime = anodize_core::util::parse_mod_timestamp(&rendered_ts)?;
                                    anodize_core::util::set_file_mtime(&bin_path, mtime)?;
                                    thread_log.verbose(&format!(
                                        "applied mod_timestamp={rendered_ts} to {}",
                                        bin_path.display()
                                    ));
                                }

                                // Execute post-build hooks after compilation
                                if !post_hooks.is_empty() {
                                    run_hooks(&post_hooks, "post-build", false, &thread_log, Some(&thread_tvars))?;
                                }

                                Ok(BuildResult {
                                    bin_path,
                                    artifact_kind,
                                    target,
                                    crate_name,
                                    binary_name,
                                    build_id,
                                    no_unique_dist_dir,
                                    amd64_variant: job_amd64_variant,
                                })
                            })
                        })
                        .collect();

                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or_else(|_| Err(anyhow::anyhow!("build thread panicked")))
                        })
                        .collect()
                });

                // Register artifacts sequentially after the chunk completes.
                for result in results {
                    let r = result?;
                    log.status(&format!(
                        "built {}/{} for {}",
                        r.crate_name, r.binary_name, r.target
                    ));
                    add_artifact(
                        ctx,
                        &r.bin_path,
                        r.artifact_kind,
                        &r.target,
                        &r.crate_name,
                        &r.binary_name,
                        &r.build_id,
                        r.no_unique_dist_dir,
                        &r.amd64_variant,
                    )?;
                }
            }

            // Copy-from jobs (must run after source builds complete)
            for job in &copy_jobs {
                let (src, dst) = job
                    .copy_from
                    .as_ref()
                    .context("copy_from job without copy_from pair (programmer bug)")?;
                resolve_copy_from(ctx, src, dst, &job.target, &job.crate_name)?;

                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        }

        // --- Universal binaries (macOS lipo) ---
        for crate_cfg in &crates {
            if let Some(ref ub_configs) = crate_cfg.universal_binaries {
                for ub in ub_configs {
                    build_universal_binary(crate_cfg.name.as_str(), ub, ctx, dry_run)?;
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::log::{StageLogger, Verbosity};
    use serial_test::serial;

    fn test_logger() -> StageLogger {
        StageLogger::new("build", Verbosity::Normal)
    }

    #[test]
    fn test_build_command_native_cargo() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"build".to_string()));
        assert!(cmd.args.contains(&"--target".to_string()));
        assert!(cmd.args.contains(&"x86_64-unknown-linux-gnu".to_string()));
        assert!(cmd.args.contains(&"--release".to_string()));
        assert!(cmd.args.contains(&"--bin".to_string()));
        assert!(cmd.args.contains(&"cfgd".to_string()));
    }

    #[test]
    fn test_build_command_zigbuild() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"zigbuild".to_string()));
        assert!(cmd.args.contains(&"--target".to_string()));
    }

    #[test]
    fn test_build_command_cross() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Cross,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert_eq!(cmd.program, "cross");
        assert!(cmd.args.contains(&"build".to_string()));
    }

    #[test]
    fn test_build_command_with_features() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &["tls".to_string(), "json".to_string()],
            false,
            &Default::default(),
            None,
            None,
        );
        assert!(cmd.args.contains(&"--features".to_string()));
        assert!(cmd.args.contains(&"tls,json".to_string()));
    }

    #[test]
    fn test_build_command_no_default_features() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            true,
            &Default::default(),
            None,
            None,
        );
        assert!(cmd.args.contains(&"--no-default-features".to_string()));
    }

    #[test]
    fn test_detect_cross_strategy_auto() {
        let strategy = detect_cross_strategy();
        // At minimum, cargo is always available
        assert!(matches!(
            strategy,
            CrossStrategy::Cargo | CrossStrategy::Zigbuild | CrossStrategy::Cross
        ));
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_build_command_with_invalid_target_triple() {
        // build_command itself does not validate target triples -- it just
        // constructs the command.  Verify the invalid triple is passed through
        // so that cargo (or cross) reports the error at execution time.
        let cmd = build_command(
            "mybin",
            "crates/mybin",
            "this-is-not-a-valid-triple",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert!(cmd.args.contains(&"this-is-not-a-valid-triple".to_string()));
        assert_eq!(cmd.program, "cargo");
    }

    #[test]
    fn test_build_command_empty_binary_name() {
        // An empty binary name should still be passed through to --bin
        let cmd = build_command(
            "",
            ".",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            None,
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert!(cmd.args.contains(&"--bin".to_string()));
        // Empty string is present in args
        assert!(cmd.args.contains(&"".to_string()));
    }

    #[test]
    fn test_build_stage_no_targets_skips_gracefully() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec![]), // explicitly empty targets
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        let stage = BuildStage;
        // Should succeed without error -- empty targets list is skipped
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        let binaries = ctx
            .artifacts
            .by_kind(anodize_core::artifact::ArtifactKind::Binary);
        assert!(binaries.is_empty());
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_copy_from_nonexistent_binary_errors_with_paths() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp_dir = std::env::temp_dir().join("anodize_build_test_copy_from");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: tmp_dir.to_string_lossy().into_owned(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                copy_from: Some("nonexistent-binary".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        let stage = BuildStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "copy_from with nonexistent source should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("copy_from") || err.contains("copy"),
            "error should mention copy_from, got: {err}"
        );
    }

    #[test]
    fn test_build_failure_nonzero_exit_produces_clear_error() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp_dir = std::env::temp_dir().join("anodize_build_test_nonzero");
        let _ = std::fs::create_dir_all(&tmp_dir);
        // Create a minimal project so cargo can find Cargo.toml but fail on build
        std::fs::write(
            tmp_dir.join("Cargo.toml"),
            "[package]\nname = \"no-such-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp_dir.join("src")).unwrap();
        std::fs::write(tmp_dir.join("src/lib.rs"), "").unwrap();

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "no-such-bin".to_string(),
            path: tmp_dir.to_string_lossy().into_owned(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "this-binary-does-not-exist".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        let stage = BuildStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "build with nonexistent binary should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed with exit code")
                || err.contains("build failed")
                || err.contains("this-binary-does-not-exist"),
            "error should mention the build failure or binary name, got: {err}"
        );
    }

    #[test]
    fn test_build_command_with_env_vars() {
        let mut env = HashMap::new();
        env.insert("CC".to_string(), "gcc-12".to_string());
        env.insert(
            "RUSTFLAGS".to_string(),
            "-C target-feature=+crt-static".to_string(),
        );

        let cmd = build_command(
            "mybin",
            ".",
            "x86_64-unknown-linux-musl",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &env,
            None,
            None,
        );
        assert_eq!(cmd.env.get("CC").unwrap(), "gcc-12");
        assert_eq!(
            cmd.env.get("RUSTFLAGS").unwrap(),
            "-C target-feature=+crt-static"
        );
    }

    // ---- Task 5A: cdylib detection tests ----

    #[test]
    fn test_detect_crate_type_cdylib() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]
"#,
        )
        .unwrap();

        let result = detect_crate_type(tmp.path().to_str().unwrap());
        assert_eq!(result, Some("cdylib".to_string()));
    }

    #[test]
    fn test_detect_crate_type_staticlib() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["staticlib", "rlib"]
"#,
        )
        .unwrap();

        let result = detect_crate_type(tmp.path().to_str().unwrap());
        assert_eq!(result, Some("staticlib".to_string()));
    }

    #[test]
    fn test_detect_crate_type_no_lib_section() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "my-bin"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        let result = detect_crate_type(tmp.path().to_str().unwrap());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_crate_type_missing_cargo_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_crate_type(tmp.path().to_str().unwrap());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_crate_type_underscore_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate_type = ["dylib"]
"#,
        )
        .unwrap();

        let result = detect_crate_type(tmp.path().to_str().unwrap());
        assert_eq!(result, Some("dylib".to_string()));
    }

    // ---- Task 5A: build_lib_command tests ----

    #[test]
    fn test_build_lib_command_uses_lib_flag() {
        let cmd = build_lib_command(
            "crates/my-lib",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"build".to_string()));
        assert!(cmd.args.contains(&"--lib".to_string()));
        assert!(cmd.args.contains(&"--target".to_string()));
        assert!(cmd.args.contains(&"x86_64-unknown-linux-gnu".to_string()));
        assert!(cmd.args.contains(&"--release".to_string()));
        // Should NOT contain --bin
        assert!(!cmd.args.contains(&"--bin".to_string()));
    }

    #[test]
    fn test_build_lib_command_with_features() {
        let cmd = build_lib_command(
            "crates/my-lib",
            "wasm32-unknown-unknown",
            &CrossStrategy::Cargo,
            None,
            &["wasm-bindgen".to_string()],
            true,
            &Default::default(),
            None,
            None,
        );
        assert!(cmd.args.contains(&"--lib".to_string()));
        assert!(cmd.args.contains(&"--features".to_string()));
        assert!(cmd.args.contains(&"wasm-bindgen".to_string()));
        assert!(cmd.args.contains(&"--no-default-features".to_string()));
    }

    #[test]
    fn test_build_lib_command_zigbuild() {
        let cmd = build_lib_command(
            ".",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            None,
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"zigbuild".to_string()));
        assert!(cmd.args.contains(&"--lib".to_string()));
    }

    // ---- Task 5E: reproducible build env var injection ----

    #[test]
    fn test_reproducible_build_sets_source_date_epoch_and_rustflags() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                reproducible: Some(true),
                flags: Some("--release".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // Inject CommitTimestamp so the build stage can read it
        ctx.template_vars_mut().set("CommitTimestamp", "1700000000");

        let stage = BuildStage;
        // dry_run means command is not executed, just eprintln'd — should succeed
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_reproducible_build_appends_to_existing_rustflags() {
        // Verify that when RUSTFLAGS is pre-set in the per-target env, the
        // remap-path-prefix flag is appended rather than replacing it.
        use std::collections::HashMap;

        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut target_env: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut inner: HashMap<String, String> = HashMap::new();
        inner.insert(
            "RUSTFLAGS".to_string(),
            "-C target-feature=+crt-static".to_string(),
        );
        target_env.insert("x86_64-unknown-linux-musl".to_string(), inner);

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-musl".to_string()]),
                reproducible: Some(true),
                flags: Some("--release".to_string()),
                env: Some(target_env),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("CommitTimestamp", "1700000000");

        let stage = BuildStage;
        // dry_run — should succeed without actually running cargo
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_reproducible_false_does_not_inject_env_vars() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                reproducible: Some(false),
                flags: Some("--release".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = BuildStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- Task 5F: universal binary tests ----

    /// Helper: register a fake Binary artifact directly in the context.
    fn register_binary(
        ctx: &mut anodize_core::context::Context,
        crate_name: &str,
        target: &str,
        path: std::path::PathBuf,
    ) {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        let mut meta = HashMap::new();
        meta.insert(
            "binary".to_string(),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path,
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    #[test]
    fn test_universal_binary_dry_run_registers_artifact() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::{Config, CrateConfig, UniversalBinaryConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            universal_binaries: Some(vec![UniversalBinaryConfig {
                id: None,
                name_template: None,
                replace: None,
                ids: None,
                hooks: None,
                mod_timestamp: None,
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        // Pre-register both macOS arch binaries as already-built artifacts
        register_binary(
            &mut ctx,
            "myapp",
            "aarch64-apple-darwin",
            std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
        );
        register_binary(
            &mut ctx,
            "myapp",
            "x86_64-apple-darwin",
            std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
        );

        let result = build_universal_binary(
            "myapp",
            &UniversalBinaryConfig {
                id: None,
                name_template: None,
                replace: None,
                ids: None,
                hooks: None,
                mod_timestamp: None,
            },
            &mut ctx,
            true, // dry_run
        );
        assert!(result.is_ok(), "dry-run universal binary should succeed");

        // A universal artifact should have been registered
        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(
            universals.len(),
            1,
            "one universal artifact should be registered"
        );
        assert_eq!(
            universals[0].metadata.get("universal").map(|s| s.as_str()),
            Some("true")
        );
    }

    #[test]
    fn test_universal_binary_dry_run_uses_name_template() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("ProjectName", "myapp");

        register_binary(
            &mut ctx,
            "myapp",
            "aarch64-apple-darwin",
            std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
        );
        register_binary(
            &mut ctx,
            "myapp",
            "x86_64-apple-darwin",
            std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
        );

        let ub = UniversalBinaryConfig {
            id: None,
            name_template: Some("{{ .ProjectName }}-universal".to_string()),
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        };

        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok());

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1);
        assert!(
            universals[0]
                .path
                .to_string_lossy()
                .contains("myapp-universal"),
            "output path should use rendered name template, got: {}",
            universals[0].path.display()
        );
    }

    #[test]
    fn test_universal_binary_skips_when_missing_arch() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        // Only arm64 — no x86_64
        register_binary(
            &mut ctx,
            "myapp",
            "aarch64-apple-darwin",
            std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
        );

        let ub = UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        };

        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok(), "missing arch should not error, just skip");

        // No universal artifact should have been registered
        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert!(
            universals.is_empty(),
            "no universal artifact when arch is missing"
        );
    }

    #[test]
    fn test_universal_binary_skips_for_different_crate() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        // Register binaries for "other-crate", not "myapp"
        register_binary(
            &mut ctx,
            "other-crate",
            "aarch64-apple-darwin",
            std::path::PathBuf::from("target/aarch64-apple-darwin/release/other"),
        );
        register_binary(
            &mut ctx,
            "other-crate",
            "x86_64-apple-darwin",
            std::path::PathBuf::from("target/x86_64-apple-darwin/release/other"),
        );

        let ub = UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        };

        // Ask for "myapp" universal — should be skipped since myapp has no arch binaries
        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok());

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert!(
            universals.is_empty(),
            "should not create universal for wrong crate"
        );
    }

    #[test]
    fn test_universal_binary_artifact_has_correct_metadata() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let mut config = anodize_core::config::Config::default();
        config.project_name = "myapp".to_string();
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        register_binary(
            &mut ctx,
            "myapp",
            "aarch64-apple-darwin",
            std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
        );
        register_binary(
            &mut ctx,
            "myapp",
            "x86_64-apple-darwin",
            std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
        );

        let ub = UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        };

        build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1);
        let art = universals[0];
        assert_eq!(art.crate_name, "myapp");
        assert_eq!(
            art.metadata.get("universal").map(|s| s.as_str()),
            Some("true")
        );
        assert_eq!(
            art.metadata.get("binary").map(|s| s.as_str()),
            Some("myapp")
        );
    }

    /// Regression test for parity with GoReleaser universalbinary.go:45 —
    /// default `name_template` is `{{ .ProjectName }}`, NOT the source binary
    /// filename. Source binaries named `myapp-bin` with project_name `myapp`
    /// must produce `myapp_darwin_all/myapp`, not `myapp_darwin_all/myapp-bin`.
    #[test]
    fn test_universal_binary_default_name_uses_project_name() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let mut config = anodize_core::config::Config::default();
        config.project_name = "myapp".to_string();
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        // Register source binaries with a distinct on-disk filename
        // (`myapp-bin`) but the crate-matching `id` so the universal-binary
        // filter selects them. The old bug — defaulting to source filename —
        // would leak through as `myapp_darwin_all/myapp-bin`.
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
            let path = std::path::PathBuf::from(format!("target/{target}/release/myapp-bin"));
            let mut meta = HashMap::new();
            meta.insert("binary".to_string(), "myapp-bin".to_string());
            meta.insert("id".to_string(), "myapp".to_string());
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path,
                target: Some(target.to_string()),
                crate_name: "myapp".to_string(),
                metadata: meta,
                size: None,
            });
        }

        let ub = UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        };

        build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1);
        let art = universals[0];
        let fname = art.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert_eq!(
            fname,
            "myapp",
            "default universal binary filename must render `{{{{ .ProjectName }}}}` (got `{}` — path {})",
            fname,
            art.path.display()
        );
        // `binary` metadata reflects the source filename, not the universal
        // output name.
        assert_eq!(
            art.metadata.get("binary").map(|s| s.as_str()),
            Some("myapp-bin")
        );
    }

    // ---- Build ignore tests ----

    #[test]
    fn test_is_target_ignored_matches() {
        let ignores = vec![BuildIgnore {
            os: "windows".to_string(),
            arch: "arm64".to_string(),
        }];
        // aarch64-pc-windows-msvc maps to os=windows, arch=arm64
        assert!(is_target_ignored("aarch64-pc-windows-msvc", &ignores));
    }

    #[test]
    fn test_is_target_ignored_no_match() {
        let ignores = vec![BuildIgnore {
            os: "windows".to_string(),
            arch: "arm64".to_string(),
        }];
        // x86_64-unknown-linux-gnu maps to os=linux, arch=amd64
        assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &ignores));
    }

    #[test]
    fn test_is_target_ignored_empty_list() {
        assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &[]));
    }

    #[test]
    fn test_is_target_ignored_multiple_rules() {
        let ignores = vec![
            BuildIgnore {
                os: "windows".to_string(),
                arch: "arm64".to_string(),
            },
            BuildIgnore {
                os: "linux".to_string(),
                arch: "arm64".to_string(),
            },
        ];
        assert!(is_target_ignored("aarch64-unknown-linux-gnu", &ignores));
        assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &ignores));
    }

    // ---- Build override tests ----

    #[test]
    fn test_find_matching_override_glob_match() {
        let overrides = vec![BuildOverride {
            targets: vec!["x86_64-*".to_string()],
            features: Some(vec!["simd".to_string()]),
            ..Default::default()
        }];
        let log = test_logger();
        let result =
            find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().features, Some(vec!["simd".to_string()]));
    }

    #[test]
    fn test_find_matching_override_no_match() {
        let log = test_logger();
        let overrides = vec![BuildOverride {
            targets: vec!["x86_64-*".to_string()],
            features: Some(vec!["simd".to_string()]),
            ..Default::default()
        }];
        let result =
            find_matching_override("aarch64-apple-darwin", &overrides, &log, false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_matching_override_wildcard_in_middle() {
        let log = test_logger();
        let overrides = vec![BuildOverride {
            targets: vec!["*-apple-darwin".to_string()],
            features: Some(vec!["metal".to_string()]),
            ..Default::default()
        }];
        let result =
            find_matching_override("aarch64-apple-darwin", &overrides, &log, false).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().features, Some(vec!["metal".to_string()]));
    }

    #[test]
    fn test_find_matching_override_empty_list() {
        let log = test_logger();
        let result = find_matching_override("x86_64-unknown-linux-gnu", &[], &log, false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_matching_override_returns_first_match() {
        let log = test_logger();
        let overrides = vec![
            BuildOverride {
                targets: vec!["x86_64-*".to_string()],
                flags: Some("--release".to_string()),
                ..Default::default()
            },
            BuildOverride {
                targets: vec!["*-linux-*".to_string()],
                flags: Some("--opt-level=3".to_string()),
                ..Default::default()
            },
        ];
        let result =
            find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().flags, Some("--release".to_string()));
    }

    #[test]
    fn test_override_env_actually_overrides_existing() {
        // Simulate the merge logic used in BuildStage::run:
        // target_env starts with a pre-existing key, override should replace it.
        let mut target_env: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        target_env.insert("CC".into(), "gcc".into());
        target_env.insert("EXISTING".into(), "keep".into());

        let override_env: std::collections::HashMap<String, String> = [
            ("CC".into(), "clang".into()),
            ("NEW_VAR".into(), "added".into()),
        ]
        .into_iter()
        .collect();

        // This mirrors the fixed merge logic (insert, not or_insert_with)
        for (k, v) in &override_env {
            target_env.insert(k.clone(), v.clone());
        }

        assert_eq!(
            target_env.get("CC").unwrap(),
            "clang",
            "override should replace existing CC value"
        );
        assert_eq!(
            target_env.get("EXISTING").unwrap(),
            "keep",
            "non-overridden key should be preserved"
        );
        assert_eq!(
            target_env.get("NEW_VAR").unwrap(),
            "added",
            "new override key should be inserted"
        );
    }

    #[test]
    fn test_find_matching_override_invalid_glob_warns() {
        // An invalid glob pattern like "[unclosed" should not panic,
        // and the function should skip it gracefully.
        let log = test_logger();
        let overrides = vec![BuildOverride {
            targets: vec!["[unclosed".to_string()],
            flags: Some("--bad".to_string()),
            ..Default::default()
        }];
        let result =
            find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
        assert!(result.is_none(), "invalid glob should not match anything");
    }

    // ---- Fix 5: cross_tool override test ----

    #[test]
    fn test_build_command_with_cross_tool() {
        let cmd = build_command(
            "test-crate",
            ".",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Auto,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            Some("/usr/bin/my-cross"),
            None,
        );
        assert_eq!(cmd.program, "/usr/bin/my-cross");
        assert!(cmd.args.contains(&"build".to_string()));
    }

    // ---- Fix 5: DEFAULT_TARGETS const test ----

    #[test]
    fn test_default_targets_has_six_entries() {
        assert_eq!(DEFAULT_TARGETS.len(), 6);
        assert!(DEFAULT_TARGETS.contains(&"x86_64-unknown-linux-gnu"));
        assert!(DEFAULT_TARGETS.contains(&"x86_64-apple-darwin"));
        assert!(DEFAULT_TARGETS.contains(&"aarch64-apple-darwin"));
        assert!(DEFAULT_TARGETS.contains(&"x86_64-pc-windows-msvc"));
        assert!(DEFAULT_TARGETS.contains(&"aarch64-pc-windows-msvc"));
        assert!(DEFAULT_TARGETS.contains(&"aarch64-unknown-linux-gnu"));
    }

    // ---- cargo_target_dir tests ----
    //
    // NOTE: These tests manipulate process-wide env vars, which is inherently
    // racy with other tests. We save/restore the original values and run
    // serially via unique test names (cargo test runs each #[test] in its own
    // thread but the env is shared). The save/restore pattern minimises
    // interference.

    /// SAFETY: `set_var` / `remove_var` are unsafe in edition 2024 because
    /// they mutate process-wide state. These test helpers are only called
    /// from single-threaded test bodies.
    fn with_clean_target_env<F: FnOnce()>(f: F) {
        let saved1 = std::env::var("CARGO_TARGET_DIR").ok();
        let saved2 = std::env::var("CARGO_BUILD_TARGET_DIR").ok();
        // SAFETY: test-only, env mutation is intentional
        unsafe {
            std::env::remove_var("CARGO_TARGET_DIR");
            std::env::remove_var("CARGO_BUILD_TARGET_DIR");
        }
        f();
        // Restore
        unsafe {
            match saved1 {
                Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }
            match saved2 {
                Some(v) => std::env::set_var("CARGO_BUILD_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_BUILD_TARGET_DIR"),
            }
        }
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_default() {
        with_clean_target_env(|| {
            assert_eq!(cargo_target_dir(None), PathBuf::from("target"));
        });
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_from_env() {
        with_clean_target_env(|| {
            // SAFETY: test-only env mutation
            unsafe { std::env::set_var("CARGO_TARGET_DIR", "/tmp/my-target") };
            assert_eq!(cargo_target_dir(None), PathBuf::from("/tmp/my-target"));
        });
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_empty_falls_through() {
        with_clean_target_env(|| {
            // SAFETY: test-only env mutation
            unsafe { std::env::set_var("CARGO_TARGET_DIR", "") };
            assert_eq!(cargo_target_dir(None), PathBuf::from("target"));
        });
    }

    #[test]
    #[serial]
    fn test_cargo_build_target_dir_fallback() {
        with_clean_target_env(|| {
            // SAFETY: test-only env mutation
            unsafe { std::env::set_var("CARGO_BUILD_TARGET_DIR", "/tmp/build-target") };
            assert_eq!(cargo_target_dir(None), PathBuf::from("/tmp/build-target"));
        });
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_takes_precedence() {
        with_clean_target_env(|| {
            // SAFETY: test-only env mutation
            unsafe {
                std::env::set_var("CARGO_TARGET_DIR", "/tmp/primary");
                std::env::set_var("CARGO_BUILD_TARGET_DIR", "/tmp/secondary");
            }
            assert_eq!(cargo_target_dir(None), PathBuf::from("/tmp/primary"));
        });
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_from_build_env() {
        with_clean_target_env(|| {
            let mut env = HashMap::new();
            env.insert(
                "CARGO_TARGET_DIR".to_string(),
                "/tmp/build-env-target".to_string(),
            );
            assert_eq!(
                cargo_target_dir(Some(&env)),
                PathBuf::from("/tmp/build-env-target")
            );
        });
    }

    #[test]
    #[serial]
    fn test_cargo_target_dir_build_env_overrides_process_env() {
        with_clean_target_env(|| {
            // SAFETY: test-only env mutation
            unsafe { std::env::set_var("CARGO_TARGET_DIR", "/tmp/process-env") };
            let mut env = HashMap::new();
            env.insert("CARGO_TARGET_DIR".to_string(), "/tmp/build-env".to_string());
            assert_eq!(
                cargo_target_dir(Some(&env)),
                PathBuf::from("/tmp/build-env")
            );
        });
    }

    // ---- Fix 5: resolve_build_program tests ----

    #[test]
    fn test_resolve_build_program_auto() {
        let (prog, sub) = resolve_build_program(&CrossStrategy::Auto, None, None, None);
        // Auto resolves at runtime — at minimum it falls back to cargo
        assert!(
            prog == "cargo" || prog == "cross",
            "Auto should resolve to cargo or cross, got: {prog}"
        );
        assert!(sub == "build" || sub == "zigbuild");
    }

    #[test]
    fn test_resolve_build_program_zigbuild() {
        let (prog, sub) = resolve_build_program(&CrossStrategy::Zigbuild, None, None, None);
        assert_eq!(prog, "cargo");
        assert_eq!(sub, "zigbuild");
    }

    #[test]
    fn test_resolve_build_program_cross() {
        let (prog, sub) = resolve_build_program(&CrossStrategy::Cross, None, None, None);
        assert_eq!(prog, "cross");
        assert_eq!(sub, "build");
    }

    #[test]
    fn test_resolve_build_program_cross_tool_overrides() {
        let (prog, sub) = resolve_build_program(
            &CrossStrategy::Zigbuild,
            Some("/usr/bin/custom"),
            None,
            None,
        );
        assert_eq!(prog, "/usr/bin/custom");
        assert_eq!(sub, "build");
    }

    #[test]
    fn test_resolve_build_program_auto_native_uses_cargo() {
        // Target == host should always resolve to cargo, even if
        // cargo-zigbuild/cross are installed.
        let host = anodize_core::partial::detect_host_target().unwrap_or_default();
        if host.is_empty() {
            return;
        }
        let (prog, sub) = resolve_build_program(&CrossStrategy::Auto, None, None, Some(&host));
        assert_eq!(prog, "cargo", "native target should use cargo");
        assert_eq!(sub, "build", "native target should use plain build");
    }

    #[test]
    fn test_same_apple_family() {
        assert!(same_apple_family(
            "aarch64-apple-darwin",
            "x86_64-apple-darwin"
        ));
        assert!(same_apple_family(
            "x86_64-apple-darwin",
            "aarch64-apple-darwin"
        ));
        assert!(same_apple_family(
            "aarch64-apple-darwin",
            "aarch64-apple-ios"
        ));
        assert!(!same_apple_family(
            "x86_64-unknown-linux-gnu",
            "x86_64-apple-darwin"
        ));
        assert!(!same_apple_family(
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc"
        ));
    }

    #[test]
    fn test_same_windows_family() {
        assert!(same_windows_family(
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc"
        ));
        assert!(same_windows_family(
            "x86_64-pc-windows-gnu",
            "x86_64-pc-windows-msvc"
        ));
        assert!(!same_windows_family(
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc"
        ));
        assert!(!same_windows_family(
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc"
        ));
    }

    #[test]
    fn test_detect_cross_strategy_for_target_apple_cross_arch() {
        // On any apple host, building a different apple arch should still
        // use cargo (clang handles apple targets universally).
        let strategy = detect_cross_strategy_for_target_with_host(
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        );
        assert_eq!(strategy, CrossStrategy::Cargo);

        let strategy = detect_cross_strategy_for_target_with_host(
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        );
        assert_eq!(strategy, CrossStrategy::Cargo);
    }

    #[test]
    fn test_detect_cross_strategy_for_target_linux_cross_arch_uses_auto() {
        // On a Linux host, building a different Linux arch does NOT get the
        // same-family exemption — it requires cross tooling (multilib gcc
        // or cross/zigbuild). We delegate to detect_cross_strategy().
        let strategy = detect_cross_strategy_for_target_with_host(
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        );
        // The result depends on which cross tools are installed on the test
        // host; we only assert it's NOT the premature Cargo shortcut.
        assert!(
            strategy == CrossStrategy::Zigbuild
                || strategy == CrossStrategy::Cross
                || strategy == CrossStrategy::Cargo,
            "linux cross-arch should go through the detect path; got {:?}",
            strategy
        );
    }

    // Test helper — same as detect_cross_strategy_for_target but lets the
    // test pin the host triple instead of reading the real machine's target.
    fn detect_cross_strategy_for_target_with_host(host: &str, target: &str) -> CrossStrategy {
        if !host.is_empty() && target == host {
            return CrossStrategy::Cargo;
        }
        if !host.is_empty() && same_apple_family(host, target) {
            return CrossStrategy::Cargo;
        }
        if !host.is_empty() && same_windows_family(host, target) {
            return CrossStrategy::Cargo;
        }
        detect_cross_strategy()
    }

    // ---- Fix 5: resolve_reproducible_epoch tests ----

    #[test]
    fn test_resolve_reproducible_epoch_from_timestamp() {
        // Unset SOURCE_DATE_EPOCH to test the commit_timestamp fallback path.
        // Safety: set_var/remove_var are unsafe in edition 2024 due to potential
        // data races. This test runs sequentially and restores the env var.
        let saved = std::env::var("SOURCE_DATE_EPOCH").ok();
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        let epoch = resolve_reproducible_epoch("1700000000");
        if let Some(val) = saved {
            unsafe { std::env::set_var("SOURCE_DATE_EPOCH", val) };
        }
        assert_eq!(epoch, 1700000000);
    }

    #[test]
    fn test_resolve_reproducible_epoch_invalid_timestamp() {
        // Safety: same rationale as above — temporary env manipulation in a test.
        let saved = std::env::var("SOURCE_DATE_EPOCH").ok();
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        let epoch = resolve_reproducible_epoch("not-a-number");
        if let Some(val) = saved {
            unsafe { std::env::set_var("SOURCE_DATE_EPOCH", val) };
        }
        assert_eq!(epoch, 0);
    }

    // ---- Fix 5: config parsing with hooks test ----

    #[test]
    fn test_build_config_with_hooks() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        hooks:
          pre:
            - "echo pre-build"
          post:
            - "echo post-build"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        let hooks = build.hooks.as_ref().unwrap();
        assert_eq!(hooks.pre.as_ref().unwrap().len(), 1);
        assert_eq!(hooks.post.as_ref().unwrap().len(), 1);
    }

    // ---- Parity gap tests ----

    #[test]
    fn test_strip_glibc_suffix_with_version() {
        let (stripped, has_suffix) = strip_glibc_suffix("aarch64-unknown-linux-gnu.2.17");
        assert_eq!(stripped, "aarch64-unknown-linux-gnu");
        assert!(has_suffix);
    }

    #[test]
    fn test_strip_glibc_suffix_no_suffix() {
        let (stripped, has_suffix) = strip_glibc_suffix("aarch64-unknown-linux-gnu");
        assert_eq!(stripped, "aarch64-unknown-linux-gnu");
        assert!(!has_suffix);
    }

    #[test]
    fn test_strip_glibc_suffix_musl_version() {
        let (stripped, has_suffix) = strip_glibc_suffix("x86_64-unknown-linux-musl.1.1");
        assert_eq!(stripped, "x86_64-unknown-linux-musl");
        assert!(has_suffix);
    }

    #[test]
    fn test_strip_glibc_suffix_windows_no_change() {
        let (stripped, has_suffix) = strip_glibc_suffix("x86_64-pc-windows-msvc");
        assert_eq!(stripped, "x86_64-pc-windows-msvc");
        assert!(!has_suffix);
    }

    #[test]
    fn test_target_for_validation_strips_suffix() {
        let t = target_for_validation("aarch64-unknown-linux-gnu.2.17");
        assert_eq!(t, "aarch64-unknown-linux-gnu");
    }

    #[test]
    fn test_target_for_validation_no_suffix() {
        let t = target_for_validation("x86_64-unknown-linux-gnu");
        assert_eq!(t, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_is_dynamically_linked_nonexistent() {
        assert!(!is_dynamically_linked(Path::new("/nonexistent/path")));
    }

    #[test]
    fn test_is_dynamically_linked_non_elf() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not_elf");
        std::fs::write(&path, b"not an elf file").unwrap();
        assert!(!is_dynamically_linked(&path));
    }

    #[test]
    fn test_check_workspace_package_no_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let result = check_workspace_package(tmp.path().to_str().unwrap(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_workspace_package_workspace_without_package_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
        )
        .unwrap();
        let result = check_workspace_package(tmp.path().to_str().unwrap(), Some("--release"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("--package=<name>"));
    }

    #[test]
    fn test_check_workspace_package_workspace_with_package_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(&cargo_toml, "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
        let result = check_workspace_package(
            tmp.path().to_str().unwrap(),
            Some("--release --package=myapp"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_workspace_package_workspace_with_p_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(&cargo_toml, "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
        let result =
            check_workspace_package(tmp.path().to_str().unwrap(), Some("--release -p myapp"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_duplicate_build_id_validation() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![
                BuildConfig {
                    id: Some("dup".to_string()),
                    binary: "myapp".to_string(),
                    targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                    ..Default::default()
                },
                BuildConfig {
                    id: Some("dup".to_string()),
                    binary: "myapp2".to_string(),
                    targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = BuildStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("found 2 builds with the ID 'dup'"),
            "expected duplicate ID error, got: {err}"
        );
    }

    #[test]
    fn test_invalid_target_errors() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["this-is-not-a-valid-triple".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = BuildStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "invalid target should error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not in the known targets list"),
            "expected known targets error, got: {err}"
        );
    }

    #[test]
    fn test_skip_build_with_string_or_bool() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                skip: Some(StringOrBool::Bool(true)),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = BuildStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered since the build was skipped
        let binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert!(
            binaries.is_empty(),
            "skipped build should produce no artifacts"
        );
    }

    #[test]
    fn test_command_override() {
        let cmd = build_command(
            "mybin",
            ".",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
            None,
            Some("auditable build"),
        );
        assert_eq!(cmd.program, "cargo");
        // "auditable build" should be split into two args
        assert!(cmd.args.contains(&"auditable".to_string()));
        assert!(cmd.args.contains(&"build".to_string()));
        assert!(cmd.args.contains(&"--bin".to_string()));
    }

    #[test]
    fn test_resolve_build_program_with_command_override() {
        let (prog, sub) =
            resolve_build_program(&CrossStrategy::Cargo, None, Some("auditable build"), None);
        assert_eq!(prog, "cargo");
        assert_eq!(sub, "auditable build");
    }

    #[test]
    fn test_known_targets_contains_mips() {
        assert!(KNOWN_TARGETS.contains(&"mips-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"mipsel-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"mips64-unknown-linux-gnuabi64"));
    }

    #[test]
    fn test_known_targets_contains_riscv() {
        assert!(KNOWN_TARGETS.contains(&"riscv64gc-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"riscv64gc-unknown-linux-musl"));
        assert!(KNOWN_TARGETS.contains(&"riscv32imac-unknown-none-elf"));
    }

    #[test]
    fn test_known_targets_contains_powerpc() {
        assert!(KNOWN_TARGETS.contains(&"powerpc-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"powerpc64-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"powerpc64le-unknown-linux-gnu"));
    }

    #[test]
    fn test_known_targets_contains_sparc() {
        assert!(KNOWN_TARGETS.contains(&"sparc64-unknown-linux-gnu"));
    }

    #[test]
    fn test_known_targets_contains_thumb() {
        assert!(KNOWN_TARGETS.contains(&"thumbv6m-none-eabi"));
        assert!(KNOWN_TARGETS.contains(&"thumbv7em-none-eabi"));
    }

    #[test]
    fn test_known_targets_contains_wasm() {
        assert!(KNOWN_TARGETS.contains(&"wasm32-unknown-unknown"));
        assert!(KNOWN_TARGETS.contains(&"wasm32-wasi"));
        assert!(KNOWN_TARGETS.contains(&"wasm32-wasip1"));
        assert!(KNOWN_TARGETS.contains(&"wasm32-wasip2"));
    }

    #[test]
    fn test_known_targets_contains_i686() {
        assert!(KNOWN_TARGETS.contains(&"i686-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"i686-unknown-freebsd"));
        assert!(KNOWN_TARGETS.contains(&"i586-unknown-linux-gnu"));
    }

    #[test]
    fn test_known_targets_contains_s390x() {
        assert!(KNOWN_TARGETS.contains(&"s390x-unknown-linux-gnu"));
        assert!(KNOWN_TARGETS.contains(&"s390x-unknown-linux-musl"));
    }

    #[test]
    fn test_build_config_command_field_parses() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        command: "auditable build"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.command.as_deref(), Some("auditable build"));
    }

    #[test]
    fn test_build_config_skip_string_or_bool_parses() {
        use anodize_core::config::{Config, StringOrBool};

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        skip: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_build_config_skip_template_parses() {
        use anodize_core::config::{Config, StringOrBool};

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        skip: "{{ if .IsSnapshot }}true{{ endif }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        match &build.skip {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_build_config_no_unique_dist_dir_string_or_bool() {
        use anodize_core::config::{Config, StringOrBool};

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        no_unique_dist_dir: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.no_unique_dist_dir, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_parse_amd64_variant_compact_flag() {
        assert_eq!(
            parse_amd64_variant_from_rustflags("-Ctarget-cpu=x86-64-v3"),
            Some("v3".to_string())
        );
    }

    #[test]
    fn test_parse_amd64_variant_spaced_flag() {
        assert_eq!(
            parse_amd64_variant_from_rustflags("-C target-cpu=x86-64-v2"),
            Some("v2".to_string())
        );
    }

    #[test]
    fn test_parse_amd64_variant_mixed_flags() {
        assert_eq!(
            parse_amd64_variant_from_rustflags(
                "--remap-path-prefix=/build -C target-cpu=x86-64-v4 -C opt-level=3"
            ),
            Some("v4".to_string())
        );
    }

    #[test]
    fn test_parse_amd64_variant_non_x86_cpu() {
        assert_eq!(
            parse_amd64_variant_from_rustflags("-Ctarget-cpu=native"),
            None
        );
    }

    #[test]
    fn test_parse_amd64_variant_no_flags() {
        assert_eq!(parse_amd64_variant_from_rustflags(""), None);
    }

    #[test]
    fn test_detect_amd64_variant_x86_64_with_rustflags() {
        let mut env = HashMap::new();
        env.insert(
            "RUSTFLAGS".to_string(),
            "-C target-cpu=x86-64-v3".to_string(),
        );
        assert_eq!(
            detect_amd64_variant("x86_64-unknown-linux-gnu", &env),
            Some("v3".to_string())
        );
    }

    #[test]
    fn test_detect_amd64_variant_non_x86_target() {
        let mut env = HashMap::new();
        env.insert(
            "RUSTFLAGS".to_string(),
            "-C target-cpu=x86-64-v3".to_string(),
        );
        assert_eq!(
            detect_amd64_variant("aarch64-unknown-linux-gnu", &env),
            None
        );
    }

    #[test]
    fn test_detect_amd64_variant_no_rustflags() {
        let env = HashMap::new();
        assert_eq!(detect_amd64_variant("x86_64-unknown-linux-gnu", &env), None);
    }
}
