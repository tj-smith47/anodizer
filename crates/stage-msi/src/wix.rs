//! WiX toolset version detection and CLI command construction.
//!
//! Hosts [`WixVersion`] (v3 `candle`+`light` vs v4 `wix build`), the
//! [`MsiCommands`] builder, the arch-name mapping, the version-resolution
//! policy (explicit config > `.wxs` namespace sniff > installed-tool probe),
//! and the `extensions:` template rendering.

use std::fs;

use anodizer_core::context::Context;

// ---------------------------------------------------------------------------
// WiX version detection
// ---------------------------------------------------------------------------

/// WiX toolset version — determines which CLI commands to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WixVersion {
    /// WiX v3: uses `candle` + `light` two-step compilation.
    V3,
    /// WiX v4: uses the unified `wix build` command.
    V4,
}

/// Commands to execute for building an MSI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsiCommands {
    /// The primary build command (V4: `wix build`, V3: `candle`).
    pub primary: Vec<String>,
    /// Optional second step (V3: `light`, V4: None).
    pub link: Option<Vec<String>>,
}

impl WixVersion {
    /// Detect the WiX version from the content of a `.wxs` file.
    ///
    /// - V3: contains the `http://schemas.microsoft.com/wix/2006/wi` namespace.
    /// - V4: contains the `http://wixtoolset.org/schemas/v4/wxs` namespace, or
    ///   no recognized namespace at all (V4 is the default for bare files).
    pub fn detect_from_wxs(content: &str) -> Self {
        if content.contains("http://schemas.microsoft.com/wix/2006/wi") {
            WixVersion::V3
        } else {
            // V4 namespace or no namespace — both default to V4
            WixVersion::V4
        }
    }

    /// Detect the WiX version from installed tools on the system.
    ///
    /// Checks for the `wix` command (V4) first, then `candle` (V3).
    /// Falls back to V4 if neither is found.
    pub fn detect_from_tools() -> Self {
        // Check for V4 first (preferred)
        if anodizer_core::util::find_binary("wix") {
            return WixVersion::V4;
        }
        // Check for V3 toolchain
        if anodizer_core::util::find_binary("candle") && anodizer_core::util::find_binary("light") {
            return WixVersion::V3;
        }
        // Default to V4
        WixVersion::V4
    }

    /// Parse a version string from config (e.g. "v3", "v4", "V3", "V4", "3", "4").
    pub fn from_config_str(s: &str) -> Option<Self> {
        let normalized = s.to_lowercase().trim_start_matches('v').to_string();
        match normalized.as_str() {
            "3" => Some(WixVersion::V3),
            "4" => Some(WixVersion::V4),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MSI command construction
// ---------------------------------------------------------------------------

/// Construct the WiX CLI commands for building an MSI.
///
/// `extensions` are WiX extension names (e.g. "WixUIExtension") that should
/// already be rendered through the template engine with empty strings filtered.
pub fn msi_command(
    wix_version: WixVersion,
    wxs_path: &str,
    output_path: &str,
    extensions: &[String],
) -> MsiCommands {
    match wix_version {
        WixVersion::V4 => {
            let mut primary = vec![
                "wix".to_string(),
                "build".to_string(),
                wxs_path.to_string(),
                "-o".to_string(),
                output_path.to_string(),
            ];
            for ext in extensions {
                primary.push("-ext".to_string());
                primary.push(ext.clone());
            }
            MsiCommands {
                primary,
                link: None,
            }
        }
        WixVersion::V3 => {
            // Derive the .wixobj path from the output path
            let wixobj_path = if let Some(prefix) = output_path.strip_suffix(".msi") {
                format!("{prefix}.wixobj")
            } else {
                format!("{output_path}.wixobj")
            };
            let mut primary = vec![
                "candle".to_string(),
                "-nologo".to_string(),
                wxs_path.to_string(),
                "-o".to_string(),
                wixobj_path.clone(),
            ];
            for ext in extensions {
                primary.push("-ext".to_string());
                primary.push(ext.clone());
            }
            let mut link = vec![
                "light".to_string(),
                "-nologo".to_string(),
                wixobj_path,
                "-o".to_string(),
                output_path.to_string(),
            ];
            // Behavioral superset: the documented usage passes `-ext` only
            // to candle. Passing the same extensions to light as well is
            // harmless (WiX ignores unused ones) but avoids link-time
            // "ExtensionRequired" errors for extensions that supply linker
            // transforms. Documented divergence — keep.
            for ext in extensions {
                link.push("-ext".to_string());
                link.push(ext.clone());
            }
            MsiCommands {
                primary,
                link: Some(link),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Architecture mapping
// ---------------------------------------------------------------------------

/// Convert a Go/Rust-style architecture name to the MSI architecture identifier.
///
/// MSI uses "x64", "x86", "arm64" in installer metadata.
pub fn map_arch_to_msi(arch: &str) -> &str {
    match arch {
        "amd64" | "x86_64" => "x64",
        "386" | "i686" | "i386" | "i586" | "x86" => "x86",
        "arm64" | "aarch64" => "arm64",
        _ => arch,
    }
}

// ---------------------------------------------------------------------------
// WiX version resolution
// ---------------------------------------------------------------------------

/// Determine the WiX toolchain version: explicit `version:` config wins,
/// otherwise sniff the `.wxs` namespace, otherwise probe installed tools.
pub(super) fn resolve_wix_version(
    msi_cfg: &anodizer_core::config::MsiConfig,
    wxs_path: &str,
    log: &anodizer_core::log::StageLogger,
) -> WixVersion {
    if let Some(ver_str) = &msi_cfg.version
        && WixVersion::from_config_str(ver_str).is_none()
    {
        log.status(&format!(
            "unrecognized WiX version '{}', auto-detecting",
            ver_str
        ));
    }
    resolve_wix_version_quiet(msi_cfg, wxs_path)
}

/// Log-free form of [`resolve_wix_version`] — identical policy (explicit
/// config > `.wxs` namespace sniff > installed-tool probe) so preflight's
/// tool requirement can never drift from the version the build would use.
pub fn resolve_wix_version_quiet(
    msi_cfg: &anodizer_core::config::MsiConfig,
    wxs_path: &str,
) -> WixVersion {
    let detect_from_wxs_or_tools = || {
        fs::read_to_string(wxs_path)
            .map(|c| WixVersion::detect_from_wxs(&c))
            .unwrap_or_else(|_| WixVersion::detect_from_tools())
    };
    msi_cfg
        .version
        .as_deref()
        .and_then(WixVersion::from_config_str)
        .unwrap_or_else(detect_from_wxs_or_tools)
}

/// Render each `extensions:` template entry through Tera, dropping empties
/// and logging (but not erroring on) per-entry render failures.
pub(super) fn render_msi_extensions(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    log: &anodizer_core::log::StageLogger,
) -> Vec<String> {
    let Some(exts) = msi_cfg.extensions.as_ref() else {
        return Vec::new();
    };
    exts.iter()
        .filter_map(|ext_tmpl| match ctx.render_template(ext_tmpl) {
            Ok(rendered) => {
                let trimmed = rendered.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            Err(e) => {
                log.warn(&format!(
                    "failed to render extension template '{}': {}",
                    ext_tmpl, e
                ));
                None
            }
        })
        .collect()
}
