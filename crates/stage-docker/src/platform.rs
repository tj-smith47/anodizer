// platform_to_arch

/// Extract the architecture component from a Docker platform string.
///
/// Handles three-component platform strings like `"linux/arm/v7"` by
/// concatenating the arch and variant (e.g. `"armv7"`), which matches
/// the output of `map_target` for armv7/armv6 Rust triples.
///
/// Examples:
/// - `"linux/amd64"` and `"linux/amd64/v3"` → `"amd64"`
/// - `"linux/arm64"` and `"linux/arm64/v8"` → `"arm64"`
/// - `"linux/arm/v7"` and the bare `"linux/arm"` → `"armv7"`
/// - `"linux/arm/v6"` → `"armv6"`
pub fn platform_to_arch(platform: &str) -> &str {
    let mut parts = platform.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(arch), Some(variant), None) => match (arch, variant) {
            ("arm", "v6") => "armv6",
            ("arm", "v7") => "armv7",
            // `linux/amd64/v3` and `linux/arm64/v8` name a level of the
            // architecture the binary already carries: the amd64 level is
            // matched from the artifact's `amd64_variant`, and every
            // aarch64 Rust build is the v8 baseline.
            ("amd64", _) | ("arm64", _) => arch,
            _ => variant,
        },
        // buildx reads a bare `linux/arm` as `linux/arm/v7`.
        (Some(_), Some("arm"), None, None) => "armv7",
        (Some(_), Some(arch), None, None) => arch,
        _ => platform,
    }
}

/// The amd64 level a platform asks for: `v1` for a bare `linux/amd64`,
/// the variant of `linux/amd64/v3`, `None` for every other architecture.
pub(crate) fn platform_amd64_level(platform: &str) -> Option<&str> {
    let mut parts = platform.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(_), Some("amd64"), None, None) => Some("v1"),
        (Some(_), Some("amd64"), Some(level), None) => Some(level),
        _ => None,
    }
}

/// Whether `artifact` is the build for `platform`: same OS and architecture
/// as the artifact's target, and on amd64 the same level, a build without an
/// `amd64_variant` being the `v1` baseline. An artifact without a target,
/// or one whose OS is `all`, belongs to every platform.
pub(crate) fn artifact_matches_platform(
    artifact: &anodizer_core::artifact::Artifact,
    platform: &str,
) -> bool {
    let Some(target) = artifact.target.as_deref() else {
        return true;
    };
    let (a_os, a_arch) = anodizer_core::target::map_target(target);
    if a_os == "all" {
        return true;
    }
    let os = platform.split('/').next().unwrap_or("linux");
    if a_os != os || a_arch != platform_to_arch(platform) {
        return false;
    }
    match platform_amd64_level(platform) {
        Some(level) => {
            artifact
                .metadata
                .get("amd64_variant")
                .map_or("v1", String::as_str)
                == level
        }
        None => true,
    }
}

/// The directory a platform's artifacts are staged under, spelled the way
/// buildx hands the platform to the Dockerfile as `$TARGETPLATFORM`: the
/// arm64 `v8` and amd64 `v1` baselines are dropped (`linux/arm64/v8` →
/// `linux/arm64`), a bare `linux/arm` gains the `v7` buildx reads it as, and
/// every other variant is kept (`linux/arm/v7`, `linux/amd64/v3`).
pub fn staging_platform_dir(platform: &str) -> String {
    let mut parts = platform.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(os), Some(arch @ ("arm64" | "amd64")), Some(variant), None)
            if (arch == "arm64" && variant == "v8") || (arch == "amd64" && variant == "v1") =>
        {
            format!("{os}/{arch}")
        }
        (Some(os), Some("arm"), None, None) => format!("{os}/arm/v7"),
        _ => platform.to_string(),
    }
}

// tag_suffix

/// Extract the architecture portion of a platform string for use as a tag suffix.
///
/// - `"linux/amd64"` → `"amd64"`
/// - `"linux/amd64/v3"` → `"amd64v3"`
/// - `"linux/arm64"` and `"linux/arm64/v8"` → `"arm64"`
/// - `"linux/arm/v7"` → `"armv7"`
pub(crate) fn tag_suffix(platform: &str) -> String {
    let arch = platform_to_arch(platform);
    match platform_amd64_level(platform) {
        Some(level) if level != "v1" => format!("{arch}{level}"),
        _ => arch.to_string(),
    }
}
