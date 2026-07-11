//! nfpm CLI command construction and format/architecture validation.

use anyhow::Result;

/// Construct the nfpm CLI command arguments.
///
/// `target` is the output file path (not directory).  When given a full file
/// path nfpm writes the package to that exact location, which avoids
/// mismatches between the predicted and actual output filename.
pub fn nfpm_command(config_path: &str, format: &str, target: &str) -> Vec<String> {
    vec![
        "nfpm".to_string(),
        "pkg".to_string(),
        "--config".to_string(),
        config_path.to_string(),
        "--packager".to_string(),
        format.to_string(),
        "--target".to_string(),
        target.to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Format validation
// ---------------------------------------------------------------------------

/// Recognized nfpm packager format names.
pub(crate) const KNOWN_FORMATS: &[&str] = &[
    "deb",
    "rpm",
    "apk",
    "archlinux",
    "termux.deb",
    "ipk",
    "msix",
];

/// Validate that a format string is a known nfpm packager.
pub(crate) fn validate_format(format: &str) -> Result<()> {
    if KNOWN_FORMATS.contains(&format) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown nfpm packager format {:?} (known: {})",
            format,
            KNOWN_FORMATS.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Architecture validation per format
// ---------------------------------------------------------------------------

/// Check if a target triple's architecture is supported for the given nfpm
/// packager format. Returns `true` for formats with no restrictions or when
/// the architecture is in the supported set.
pub(crate) fn is_arch_supported_for_format(triple: &str, format: &str) -> bool {
    // Extract architecture component from triple
    let first = triple.split('-').next().unwrap_or("");

    match format {
        "archlinux" => {
            // Archlinux only supports: x86_64, i686, aarch64, armv7h
            matches!(first, "x86_64" | "i686" | "aarch64" | "armv7" | "armv7l")
        }
        "termux.deb" => {
            // Termux (Android): aarch64, arm, i686, x86_64
            matches!(
                first,
                "aarch64" | "arm" | "armv7" | "armv7l" | "armv6" | "armv6l" | "i686" | "x86_64"
            )
        }
        // All other formats (deb, rpm, apk, ipk) have broad arch support
        _ => true,
    }
}
