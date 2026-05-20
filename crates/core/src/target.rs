// Build target mapping: triple -> (OS, arch) for archive naming.
//
// This is the canonical mapping used by all stages.  The values returned here
// must match what publish stages (AUR, Homebrew, Krew, winget, etc.) expect
// so that `infer_os`/`infer_arch` in `stage-publish` are a strict subset of
// what this function handles.

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
            "i686" | "i386" | "i586" => "386",
            "armv7" | "armv7l" => "armv7",
            "armv6" | "armv6l" | "arm" => "armv6",
            "s390x" => "s390x",
            "ppc64le" | "powerpc64le" => "ppc64le",
            "ppc64" | "powerpc64" => "ppc64",
            "riscv64gc" | "riscv64" => "riscv64",
            "mips64" | "mips64el" => first,
            "mips" | "mipsel" => first,
            "loongarch64" => "loong64",
            other => other,
        }
    };

    (os.to_string(), arch.to_string())
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
