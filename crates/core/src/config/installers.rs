use super::*;

// ---------------------------------------------------------------------------
// DmgConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DmgConfig {
    /// Unique identifier for this DMG config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output DMG filename (supports templates).
    pub name: Option<String>,
    /// Additional files to include in the DMG (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Remove source archives from artifacts, keeping only DMG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this DMG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Which artifact type to package: "binary" (default) or "appbundle".
    #[serde(rename = "use")]
    pub use_: Option<String>,
    /// Template-conditional: skip this DMG config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// MsiConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MsiConfig {
    /// Unique identifier for this MSI config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Path to the WiX source file (.wxs). Goes through template engine. Required.
    pub wxs: Option<String>,
    /// Output MSI filename (supports templates).
    pub name: Option<String>,
    /// WiX schema version: v3 or v4 (auto-detected from .wxs if omitted).
    pub version: Option<String>,
    /// Remove source archives from artifacts, keeping only MSI.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this MSI config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Additional files available in the WiX build context (simple filenames).
    pub extra_files: Option<Vec<String>>,
    /// WiX extensions to enable (e.g., "WixUIExtension"). Templates allowed.
    pub extensions: Option<Vec<String>>,
    /// Template-conditional: skip this MSI config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Pre/post MSI-build hooks (GoReleaser Pro v2.14+). Accepts `pre`/`post`
    /// or `before`/`after` via BuildHooksConfig's serde aliases. Runs before
    /// / after candle+light for each matched artifact.
    pub hooks: Option<BuildHooksConfig>,
}

// ---------------------------------------------------------------------------
// PkgConfig (macOS .pkg installer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PkgConfig {
    /// Unique identifier for this PKG config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Package identifier in reverse-domain notation (e.g. com.example.myapp). Required.
    pub identifier: Option<String>,
    /// Output PKG filename (supports templates).
    pub name: Option<String>,
    /// Installation path. Default: /usr/local/bin.
    pub install_location: Option<String>,
    /// Path to scripts directory containing preinstall/postinstall scripts.
    pub scripts: Option<String>,
    /// Additional files to include in the package (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Remove source archives from artifacts, keeping only PKG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Which artifact type to package: "binary" (default) or "appbundle".
    #[serde(rename = "use")]
    pub use_: Option<String>,
    /// Minimum macOS version (e.g. "10.13"). Forwarded to `productbuild --min-os-version`.
    pub min_os_version: Option<String>,
    /// Skip this PKG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional: skip this PKG config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// NsisConfig (Windows NSIS installer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NsisConfig {
    /// Unique identifier for this NSIS config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output installer filename (supports templates).
    pub name: Option<String>,
    /// Path to the NSIS script template (.nsi). Goes through template engine.
    pub script: Option<String>,
    /// Additional files to include alongside the installer (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Skip this NSIS config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Remove source archives from artifacts, keeping only the installer.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Template-conditional: skip this NSIS config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// AppBundleConfig (macOS .app bundle)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AppBundleConfig {
    /// Unique identifier for this app bundle config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output .app bundle name (supports templates).
    pub name: Option<String>,
    /// Path to .icns icon file for the app bundle (supports templates).
    pub icon: Option<String>,
    /// Bundle identifier in reverse-DNS notation (e.g. com.example.myapp). Required.
    pub bundle: Option<String>,
    /// Additional files to include in the bundle (src/dst/info objects or glob strings).
    pub extra_files: Option<Vec<ArchiveFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Remove source archives from artifacts, keeping only the app bundle.
    pub replace: Option<bool>,
    /// Skip this app bundle config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional: skip this app bundle config if rendered result is
    /// "false" or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// FlatpakConfig (Linux Flatpak bundle)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct FlatpakConfig {
    /// Unique identifier for this Flatpak config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output .flatpak filename (supports templates).
    pub name_template: Option<String>,
    /// Flatpak application ID in reverse-DNS notation (e.g. org.example.MyApp). Required.
    pub app_id: Option<String>,
    /// Flatpak runtime (e.g. org.freedesktop.Platform). Required.
    pub runtime: Option<String>,
    /// Flatpak runtime version (e.g. "24.08"). Required.
    pub runtime_version: Option<String>,
    /// Flatpak SDK (e.g. org.freedesktop.Sdk). Required.
    pub sdk: Option<String>,
    /// Command to run inside the Flatpak sandbox. Defaults to first binary name.
    pub command: Option<String>,
    /// Sandbox permissions (e.g. --share=network, --socket=x11).
    pub finish_args: Option<Vec<String>>,
    /// Additional files to include alongside the binary (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Remove source archives from artifacts, keeping only the Flatpak bundle.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this Flatpak config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}
