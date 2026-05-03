use std::collections::HashMap;

use anodizer_core::config::{BuildIgnore, BuildOverride};
use anodizer_core::target::map_target;

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

/// Compile a glob pattern with consistent strict-mode-vs-warn handling
/// across the build stage's pattern-matching call sites (per-target env
/// keys, build override `targets`, …).
///
/// Returns `Ok(None)` when the pattern fails to compile in normal mode
/// (after logging a warning); `Err` in strict mode; `Ok(Some(pat))` on
/// success. `label` describes the configuration site that produced the
/// pattern, e.g. `"build.env key"` or `"build override target"`.
pub(crate) fn try_compile_glob(
    key: &str,
    label: &str,
    log: &anodizer_core::log::StageLogger,
    strict: bool,
) -> anyhow::Result<Option<glob::Pattern>> {
    match glob::Pattern::new(key) {
        Ok(pat) => Ok(Some(pat)),
        Err(e) => {
            if strict {
                anyhow::bail!(
                    "build: invalid glob pattern in {} '{}': {} (strict mode)",
                    label,
                    key,
                    e
                );
            }
            log.warn(&format!(
                "invalid glob pattern in {} '{}': {}",
                label, key, e
            ));
            Ok(None)
        }
    }
}

/// Resolve the merged env map for a build target by interpreting each
/// `build.env` key as a glob pattern (matching the same `glob::Pattern`
/// semantic used by `find_matching_override` and the `targets:` filter on
/// upx / overrides).
///
/// Tradeoff-free UX win over the previous exact-key lookup: a user who writes
/// `env: { "*-linux-gnu": { CC: musl-gcc } }` now gets that env applied to
/// every linux-gnu target instead of silently nothing. Exact target strings
/// are valid trivial globs and continue to match exactly as before.
///
/// **Merge order is alphabetic, not most-specific-wins.** Keys are visited in
/// lexicographic order; later (alphabetically-greater) matching keys override
/// earlier ones on conflicting values. With both `*-linux-gnu` and the exact
/// target string matching, the exact key sorts later and wins coincidentally.
/// For two glob keys (e.g. `*-linux-gnu` and `x86_64-*`), ASCII order — not
/// pattern specificity — decides. Authors of multiple overlapping keys must
/// keep that in mind; prefer non-overlapping patterns or rely on the exact
/// target string to override globs.
///
/// Returns `Ok(None)` when the env map is absent / empty / has no matching
/// keys; otherwise `Ok(Some(merged))`.
pub(crate) fn resolve_target_env(
    env: Option<&HashMap<String, HashMap<String, String>>>,
    target: &str,
    log: &anodizer_core::log::StageLogger,
    strict: bool,
) -> anyhow::Result<Option<HashMap<String, String>>> {
    let Some(env) = env else { return Ok(None) };
    if env.is_empty() {
        return Ok(None);
    }
    let mut sorted_keys: Vec<&String> = env.keys().collect();
    sorted_keys.sort();
    let mut merged: HashMap<String, String> = HashMap::new();
    let mut matched_any = false;
    for key in sorted_keys {
        let Some(pat) = try_compile_glob(key, "build.env key", log, strict)? else {
            continue;
        };
        if pat.matches(target)
            && let Some(vals) = env.get(key)
        {
            matched_any = true;
            for (k, v) in vals {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(if matched_any { Some(merged) } else { None })
}

/// Find the first matching override for a target triple.
/// Override `targets` are glob patterns matched against the full triple string.
pub(crate) fn find_matching_override<'a>(
    target: &str,
    overrides: &'a [BuildOverride],
    log: &anodizer_core::log::StageLogger,
    strict: bool,
) -> anyhow::Result<Option<&'a BuildOverride>> {
    for ov in overrides {
        for pat_str in &ov.targets {
            let Some(pat) = try_compile_glob(pat_str, "build override target", log, strict)? else {
                continue;
            };
            if pat.matches(target) {
                return Ok(Some(ov));
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Default targets — used when neither build.targets nor defaults.targets is set
// ---------------------------------------------------------------------------

pub(crate) const DEFAULT_TARGETS: &[&str] = &[
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

pub(crate) const KNOWN_TARGETS: &[&str] = &[
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
