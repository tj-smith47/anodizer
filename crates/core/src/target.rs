// Build target mapping: triple -> (OS, arch) for archive naming.
//
// This is the canonical mapping used by all stages.  The values returned here
// must match what publish stages (AUR, Homebrew, Krew, winget, etc.) expect
// so that `infer_os`/`infer_arch` in `stage-publish` are a strict subset of
// what this function handles.

/// Default build-target matrix used when neither a build's `targets:` nor
/// `defaults.targets` is configured. Single source of truth shared by the
/// build stage's job planner and preflight's target derivation.
pub const DEFAULT_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
];

/// The one Rust-arch-token → Go/OCI arch-name table.
///
/// Accepts both vocabularies that carry a Rust architecture token: target
/// triple first components (`powerpc64le`, `mips64el`, `riscv64gc`, …), which
/// spell endianness explicitly, and `std::env::consts::ARCH` values
/// (`powerpc64`, `mips64`, …), which do NOT — Rust's `target_arch` is the same
/// string for both endiannesses. `little_endian` disambiguates those
/// endian-ambiguous tokens: pass `cfg!(target_endian = "little")` when mapping
/// the host's own `ARCH`, and `false` when mapping a triple component (a
/// little-endian triple always spells it in the token itself).
///
/// Docker/OCI `platform.architecture` values are GOARCH values, so this single
/// table serves template `Goarch` vars, triple-derived asset naming, and
/// container platform pinning — the three consumers whose former private
/// copies disagreed on `powerpc64` and `loongarch64`.
///
/// 32-bit ARM tokens (`arm`, `armv6`, `armv7`, …) are deliberately absent:
/// GOARCH for all of them is plain `"arm"`, while [`map_target`] needs the
/// composite `armv6`/`armv7` archive-naming tokens — the one place the two
/// vocabularies genuinely differ, so each caller keeps its own ARM handling.
///
/// Returns `None` for tokens with no known Go arch name (`wasm32`, a typo, a
/// user-supplied prebuilt token) so callers choose their own fallthrough.
pub fn rust_arch_to_goarch(token: &str, little_endian: bool) -> Option<&'static str> {
    let mapped = match token {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        "x86" | "i686" | "i386" | "i586" => "386",
        "s390x" => "s390x",
        "riscv64" | "riscv64gc" => "riscv64",
        "loongarch64" | "loong64" => "loong64",
        "sparcv9" | "sparc64" => "sparc64",
        "powerpc64le" | "ppc64le" => "ppc64le",
        "powerpc64" | "ppc64" => {
            if little_endian {
                "ppc64le"
            } else {
                "ppc64"
            }
        }
        "mipsel" => "mipsel",
        "mips64el" => "mips64el",
        "mips" => {
            if little_endian {
                "mipsel"
            } else {
                "mips"
            }
        }
        "mips64" => {
            if little_endian {
                "mips64el"
            } else {
                "mips64"
            }
        }
        _ => return None,
    };
    Some(mapped)
}

pub fn map_target(triple: &str) -> (String, String) {
    // ---- OS (substring match) ----
    // Note: android triples contain "linux" (e.g. aarch64-linux-android),
    // so check android before linux.
    let os = if triple.contains("android") {
        "android"
    } else if triple.contains("ios") {
        "ios"
    } else if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") || triple.contains("apple") {
        "darwin"
    } else if triple.contains("windows") {
        "windows"
    } else if triple.contains("freebsd") {
        "freebsd"
    } else if triple.contains("netbsd") {
        "netbsd"
    } else if triple.contains("openbsd") {
        "openbsd"
    } else if triple.contains("aix") {
        "aix"
    } else if triple.contains("solaris") {
        "solaris"
    } else if triple.contains("illumos") {
        "illumos"
    } else {
        "unknown"
    };

    // ---- Architecture ----
    // First check contains-based patterns (matches util.rs infer_arch behaviour),
    // then fall back to exact first-component matching for Rust-specific arch names.
    //
    // Special case: synthetic "darwin-universal" triple registered for lipo'd
    // macOS universal binaries. There's no real CPU here — emit "all" so
    // publishers (krew especially) can fan it out to amd64+arm64 entries via
    // their `arch == "all"` handling, and so archive naming produces
    // `<name>-darwin-all.<ext>` instead of `<name>-darwin-darwin.<ext>`.
    let arch = if triple == "darwin-universal" {
        "all"
    } else if triple.contains("x86_64") || triple.contains("amd64") {
        "amd64"
    } else if triple.contains("aarch64") || triple.contains("arm64") {
        "arm64"
    } else {
        let first = triple.split('-').next().unwrap_or("unknown");
        match first {
            // Archive naming carries the composite armv6/armv7 token (GOARCH
            // would be plain "arm"), so 32-bit ARM stays outside the shared
            // goarch table.
            "armv7" | "armv7l" => "armv7",
            "armv6" | "armv6l" | "arm" => "armv6",
            // Triple components spell endianness explicitly (mips64el,
            // powerpc64le), so the endian-ambiguous-host disambiguation is off.
            other => rust_arch_to_goarch(other, false).unwrap_or(other),
        }
    };

    (os.to_string(), arch.to_string())
}

/// Map a target triple to its libc family for the `{{ .Libc }}` template var.
///
/// Returns the triple's libc-environment component using the same spelling
/// the codebase already uses for triples (`musl` for `*-musl`, `gnu` for
/// `*-gnu*`). Targets with no libc concept (macOS, Windows, bare-metal)
/// return an empty string so `conflicts`/`provides` templates that branch on
/// `{{ .Libc }}` degrade cleanly to the non-libc value.
pub fn libc_from_target(triple: &str) -> &'static str {
    if triple.contains("musl") {
        "musl"
    } else if triple.contains("gnu") {
        "gnu"
    } else {
        ""
    }
}

/// A target triple whose architecture has no known Debian (`dpkg`)
/// architecture name.
///
/// Carries the offending input so the caller can build an actionable hard-fail
/// message naming the triple that could not be mapped — never a silent
/// fallthrough to a raw triple fragment that would mis-index the `.deb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownDebianArch {
    /// The triple (or arch token) that could not be mapped to a Debian arch.
    pub input: String,
}

impl std::fmt::Display for UnknownDebianArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no Debian (dpkg) architecture name is known for target '{}' \
             — set an explicit `deb_architecture:` override on the Artifactory \
             entry (e.g. `deb_architecture: \"amd64\"`) to name the repository \
             slice for this build",
            self.input
        )
    }
}

impl std::error::Error for UnknownDebianArch {}

/// Map a target triple to its Debian architecture name (the value apt and the
/// Debian repository index use), e.g. `amd64`, `arm64`, `armhf`, `i386`.
///
/// This differs from [`map_target`]'s GoReleaser-style arch in the names
/// Debian spells differently: `386` → `i386`, `armv7` → `armhf`,
/// `armv6` → `armel`, `ppc64le` → `ppc64el`. The result is suitable for the
/// `deb.architecture=` Artifactory matrix param so an uploaded `.deb` lands in
/// the correct architecture slice of the repo index.
///
/// *Fallible by design*: an architecture with no known Debian spelling returns
/// [`UnknownDebianArch`] rather than relabeling it with a raw triple fragment.
/// A user-supplied `prebuilt` target can carry an arbitrary arch token, and a
/// silent wrong `deb.architecture=` value would mis-index the `.deb` into the
/// wrong (or an empty-named) repository slice — exactly the silent-wrong-value
/// failure this stage exists to prevent. Mirrors the fallible pattern of
/// `aur_arch::triple_to_pacman_arch`.
pub fn debian_arch_from_target(triple: &str) -> Result<String, UnknownDebianArch> {
    let (_, arch) = map_target(triple);
    debian_arch_from_arch(&arch)
        .map(str::to_string)
        .ok_or_else(|| UnknownDebianArch {
            input: triple.to_string(),
        })
}

/// Map a GoReleaser-style arch name (as produced by [`map_target`]) to its
/// Debian architecture spelling, or `None` when the arch has no known Debian
/// equivalent.
///
/// Split out from [`debian_arch_from_target`] so callers that already hold the
/// `(os, arch)` pair don't re-parse the triple. The recognized set is the
/// Debian `dpkg` architecture names anodizer can build for: every arch
/// [`map_target`] produces for a Linux target is enumerated here; anything else
/// (a `darwin-universal` synthetic, an unmapped `prebuilt` token, a typo)
/// returns `None` so the caller hard-fails instead of shipping a wrong slice.
pub fn debian_arch_from_arch(arch: &str) -> Option<&'static str> {
    let mapped = match arch {
        "amd64" => "amd64",
        "arm64" => "arm64",
        "386" => "i386",
        "armv7" => "armhf",
        "armv6" => "armel",
        "ppc64le" => "ppc64el",
        "ppc64" => "ppc64",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        "mips64" => "mips64",
        "mips64el" => "mips64el",
        "mips" => "mips",
        "mipsel" => "mipsel",
        "sparc64" => "sparc64",
        "loong64" => "loong64",
        _ => return None,
    };
    Some(mapped)
}

/// Returns `true` if the target triple represents a macOS (Darwin) target.
pub fn is_darwin(triple: &str) -> bool {
    triple.contains("darwin") || triple.contains("apple")
}

/// Returns `true` if the target triple represents a Linux target.
///
/// Excludes Android, which also contains "linux" in the triple.
pub fn is_linux(triple: &str) -> bool {
    triple.contains("linux") && !triple.contains("android")
}

/// Returns `true` if the target triple represents a Windows target.
pub fn is_windows(triple: &str) -> bool {
    triple.contains("windows")
}

/// Returns `true` if the target triple is a Windows-MSVC target
/// (e.g. `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`).
///
/// MSVC targets are distinguished from `*-windows-gnu` because they cannot
/// be cross-compiled off a Windows host: they need the MSVC SDK / CRT
/// headers (e.g. `assert.h`) that cargo-zigbuild does not bundle, whereas
/// `*-windows-gnu` links against the MinGW runtime zig ships and builds
/// from any host.
pub fn is_windows_msvc(triple: &str) -> bool {
    triple.contains("windows-msvc")
}

/// Returns `true` if the target triple represents an iOS target.
pub fn is_ios(triple: &str) -> bool {
    triple.contains("ios")
}

/// Returns `true` if the target triple represents an AIX target.
pub fn is_aix(triple: &str) -> bool {
    triple.contains("aix")
}

/// Returns `true` if the target triple is eligible for nfpm packaging.
///
/// nfpm filters artifacts by
/// `ByGooses("linux", "ios", "android", "aix")`.
pub fn is_nfpm_target(triple: &str) -> bool {
    is_linux(triple) || is_ios(triple) || triple.contains("android") || is_aix(triple)
}

/// Map an optional target triple to `(os, arch)` strings, falling back to
/// `(default_os, "amd64")` when no triple is supplied.
///
/// Each platform-specific stage (DMG, AppBundle, NSIS, Flatpak) needs the
/// same lookup but with its own platform-specific default OS — DMG and
/// AppBundle default to darwin, NSIS to windows, Flatpak to linux. Sharing
/// the call site here keeps the default-OS list discoverable in one place.
pub fn os_arch_with_default(target: Option<&str>, default_os: &str) -> (String, String) {
    target
        .map(map_target)
        .unwrap_or_else(|| (default_os.to_string(), "amd64".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_to_os_arch() {
        let (os, arch) = map_target("x86_64-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_darwin_arm64() {
        let (os, arch) = map_target("aarch64-apple-darwin");
        assert_eq!(os, "darwin");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_windows() {
        let (os, arch) = map_target("x86_64-pc-windows-msvc");
        assert_eq!(os, "windows");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_is_windows_msvc() {
        assert!(is_windows_msvc("x86_64-pc-windows-msvc"));
        assert!(is_windows_msvc("aarch64-pc-windows-msvc"));
        assert!(!is_windows_msvc("x86_64-pc-windows-gnu"));
        assert!(!is_windows_msvc("x86_64-unknown-linux-gnu"));
        assert!(!is_windows_msvc("aarch64-apple-darwin"));
    }

    #[test]
    fn test_riscv64() {
        let (os, arch) = map_target("riscv64gc-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "riscv64");
    }

    #[test]
    fn test_i686() {
        let (os, arch) = map_target("i686-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "386");
    }

    #[test]
    fn test_armv7() {
        let (os, arch) = map_target("armv7-unknown-linux-gnueabihf");
        assert_eq!(os, "linux");
        assert_eq!(arch, "armv7");
    }

    #[test]
    fn test_freebsd() {
        let (os, arch) = map_target("x86_64-unknown-freebsd");
        assert_eq!(os, "freebsd");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_s390x() {
        let (os, arch) = map_target("s390x-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "s390x");
    }

    #[test]
    fn test_ppc64le() {
        let (os, arch) = map_target("powerpc64le-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "ppc64le");
    }

    #[test]
    fn test_android() {
        let (os, arch) = map_target("aarch64-linux-android");
        assert_eq!(os, "android");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_linux_musl() {
        let (os, arch) = map_target("aarch64-unknown-linux-musl");
        assert_eq!(os, "linux");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_unknown_target() {
        let (os, arch) = map_target("wasm32-unknown-unknown");
        assert_eq!(os, "unknown");
        assert_eq!(arch, "wasm32");
    }

    #[test]
    fn test_ios() {
        let (os, arch) = map_target("aarch64-apple-ios");
        assert_eq!(os, "ios");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_aix() {
        let (os, arch) = map_target("powerpc64-ibm-aix");
        assert_eq!(os, "aix");
        assert_eq!(arch, "ppc64");
    }

    #[test]
    fn test_solaris_sparcv9() {
        let (os, arch) = map_target("sparcv9-sun-solaris");
        assert_eq!(os, "solaris");
        assert_eq!(arch, "sparc64");
    }

    #[test]
    fn test_sparc64_linux() {
        let (os, arch) = map_target("sparc64-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "sparc64");
    }

    #[test]
    fn test_illumos() {
        let (os, arch) = map_target("x86_64-unknown-illumos");
        assert_eq!(os, "illumos");
        assert_eq!(arch, "amd64");
    }

    /// Every non-ARM token the shared goarch table covers must agree with
    /// `map_target`'s triple-derived arch — the agreement that formerly held
    /// only by parallel-maintained match tables (and had already broken on
    /// powerpc64 / loongarch64 between the private copies).
    #[test]
    fn test_goarch_table_agrees_with_map_target_on_every_token() {
        let tokens = [
            "x86_64",
            "amd64",
            "aarch64",
            "arm64",
            "i686",
            "i386",
            "i586",
            "s390x",
            "riscv64",
            "riscv64gc",
            "loongarch64",
            "loong64",
            "sparcv9",
            "sparc64",
            "powerpc64",
            "ppc64",
            "powerpc64le",
            "ppc64le",
            "mips",
            "mipsel",
            "mips64",
            "mips64el",
        ];
        for token in tokens {
            let expected = rust_arch_to_goarch(token, false)
                .unwrap_or_else(|| panic!("table must cover {token}"));
            let (_, arch) = map_target(&format!("{token}-unknown-linux-gnu"));
            assert_eq!(
                arch, expected,
                "map_target and rust_arch_to_goarch must agree on '{token}'"
            );
        }
    }

    #[test]
    fn test_goarch_endian_disambiguation() {
        // Rust's env ARCH is "powerpc64" / "mips64" for BOTH endiannesses;
        // only the endian flag tells them apart. Go's runtime GOARCH on a
        // little-endian POWER host is ppc64le, never ppc64.
        assert_eq!(rust_arch_to_goarch("powerpc64", true), Some("ppc64le"));
        assert_eq!(rust_arch_to_goarch("powerpc64", false), Some("ppc64"));
        assert_eq!(rust_arch_to_goarch("mips64", true), Some("mips64el"));
        assert_eq!(rust_arch_to_goarch("mips64", false), Some("mips64"));
        assert_eq!(rust_arch_to_goarch("mips", true), Some("mipsel"));
        // Explicitly-little tokens are little regardless of the flag.
        assert_eq!(rust_arch_to_goarch("powerpc64le", false), Some("ppc64le"));
        assert_eq!(rust_arch_to_goarch("mips64el", false), Some("mips64el"));
    }

    #[test]
    fn test_goarch_table_excludes_arm_and_unknowns() {
        // 32-bit ARM is the deliberate vocabulary split: GOARCH is "arm" while
        // map_target needs armv6/armv7 — each caller keeps its own handling.
        assert_eq!(rust_arch_to_goarch("arm", true), None);
        assert_eq!(rust_arch_to_goarch("armv7", false), None);
        assert_eq!(rust_arch_to_goarch("wasm32", false), None);
        assert_eq!(rust_arch_to_goarch("frob", false), None);
    }

    #[test]
    fn test_libc_from_target() {
        assert_eq!(libc_from_target("x86_64-unknown-linux-musl"), "musl");
        assert_eq!(libc_from_target("aarch64-unknown-linux-musl"), "musl");
        assert_eq!(libc_from_target("x86_64-unknown-linux-gnu"), "gnu");
        assert_eq!(libc_from_target("armv7-unknown-linux-gnueabihf"), "gnu");
        // No libc concept — empty so templates degrade cleanly.
        assert_eq!(libc_from_target("x86_64-apple-darwin"), "");
        assert_eq!(libc_from_target("x86_64-pc-windows-msvc"), "");
        assert_eq!(libc_from_target("x86_64-pc-windows-gnu"), "gnu");
    }

    #[test]
    fn test_debian_arch_from_target() {
        // Names Debian spells differently from GoReleaser-style arch.
        assert_eq!(
            debian_arch_from_target("x86_64-unknown-linux-gnu").unwrap(),
            "amd64"
        );
        assert_eq!(
            debian_arch_from_target("aarch64-unknown-linux-gnu").unwrap(),
            "arm64"
        );
        assert_eq!(
            debian_arch_from_target("armv7-unknown-linux-gnueabihf").unwrap(),
            "armhf"
        );
        assert_eq!(
            debian_arch_from_target("i686-unknown-linux-gnu").unwrap(),
            "i386"
        );
        assert_eq!(
            debian_arch_from_target("arm-unknown-linux-gnueabi").unwrap(),
            "armel"
        );
        assert_eq!(
            debian_arch_from_target("powerpc64le-unknown-linux-gnu").unwrap(),
            "ppc64el"
        );
        // Names already matching Debian pass through unchanged.
        assert_eq!(
            debian_arch_from_target("s390x-unknown-linux-gnu").unwrap(),
            "s390x"
        );
        assert_eq!(
            debian_arch_from_target("riscv64gc-unknown-linux-gnu").unwrap(),
            "riscv64"
        );
        assert_eq!(
            debian_arch_from_target("loongarch64-unknown-linux-gnu").unwrap(),
            "loong64"
        );
    }

    #[test]
    fn test_debian_arch_unmapped_triple_hard_errors() {
        // An unmapped / exotic / user-supplied prebuilt triple must HARD-ERROR
        // with the offending triple, never silently relabel a raw fragment as
        // the deb architecture (which would mis-index the .deb).
        // Note: deb-arch derivation is arch-only (map_target ignores the OS
        // for the arch token), and the deb stage only ever feeds Linux deb
        // artifacts here — so these are tokens map_target leaves unmapped
        // (`frob`, `wasm32`) or the `all` synthetic, none of which is a
        // dpkg architecture.
        for bad in [
            "frob-unknown-linux-gnu",
            "wasm32-unknown-unknown",
            "darwin-universal",
        ] {
            let err = debian_arch_from_target(bad).expect_err(&format!("'{bad}' must be rejected"));
            assert_eq!(err.input, bad, "error carries the offending triple");
            let msg = err.to_string();
            assert!(msg.contains(bad), "message quotes the bad triple: {msg}");
            assert!(
                msg.contains("deb_architecture"),
                "message names the override field as the fix: {msg}"
            );
        }
    }

    #[test]
    fn test_is_nfpm_target() {
        assert!(is_nfpm_target("x86_64-unknown-linux-gnu"));
        assert!(is_nfpm_target("aarch64-linux-android"));
        assert!(is_nfpm_target("aarch64-apple-ios"));
        assert!(is_nfpm_target("powerpc64-ibm-aix"));
        assert!(!is_nfpm_target("x86_64-apple-darwin"));
        assert!(!is_nfpm_target("x86_64-pc-windows-msvc"));
        assert!(!is_nfpm_target("x86_64-unknown-freebsd"));
    }
}
