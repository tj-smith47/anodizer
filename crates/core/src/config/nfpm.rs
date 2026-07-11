use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Amd64Variant, FileInfo, StringOrU32};

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmConfig {
    /// Unique identifier for cross-referencing this nFPM config.
    pub id: Option<String>,
    /// Package name (defaults to crate name).
    pub package_name: Option<String>,
    /// Package formats to produce: deb, rpm, apk, archlinux, termux.deb, ipk,
    /// msix (at least one required).
    pub formats: Vec<String>,
    /// Package vendor name — the distributing entity recorded in the
    /// rpm/deb Vendor field. When unset, derived from the crate's first
    /// `Cargo.toml [package].authors` entry with any `<email>` suffix
    /// stripped (e.g. `"Ada Lovelace <ada@x>"` → `"Ada Lovelace"`).
    pub vendor: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Package maintainer in "Name <email>" format.
    pub maintainer: Option<String>,
    /// Package description (multiline supported).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Installation directory for binaries (default: /usr/bin).
    pub bindir: Option<String>,
    /// Rename the installed binary inside the package only.
    ///
    /// When set, the auto-emitted binary content entry is installed under this
    /// name (in `bindir`) instead of the built file's name; the archive/build
    /// output is untouched. Use this to resolve Debian/RPM name clashes — e.g.
    /// `fd` ships its binary as `fdfind` in the Debian package while the tarball
    /// keeps `fd`. Templated.
    pub bin_alias: Option<String>,
    /// Files to include in the package beyond the main binary.
    pub contents: Option<Vec<NfpmContent>>,
    /// Runtime package dependencies keyed by format (e.g., {"deb": ["libc6"], "rpm": ["glibc"]}).
    pub dependencies: Option<HashMap<String, Vec<String>>>,
    /// Per-format setting overrides (e.g., {"deb": {compression: "xz"}}).
    pub overrides: Option<HashMap<String, serde_json::Value>>,
    /// Package filename template (supports templates).
    pub file_name_template: Option<String>,
    /// Package lifecycle scripts (preinstall, postinstall, preremove, postremove).
    pub scripts: Option<NfpmScripts>,
    /// Packages recommended (soft dependency) by this package.
    pub recommends: Option<Vec<String>>,
    /// Packages suggested (weaker than recommends) by this package.
    pub suggests: Option<Vec<String>>,
    /// Packages this package conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Packages this package replaces (for upgrade paths from old package names).
    pub replaces: Option<Vec<String>>,
    /// Virtual packages provided by this package.
    pub provides: Option<Vec<String>>,
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    /// Accepts the deprecated `builds:` spelling via serde alias for
    /// back-compat with imported configs (the legacy `builds` key
    /// marked `deprecated`, aliasing `ids`).
    #[serde(alias = "builds")]
    pub ids: Option<Vec<String>>,
    /// amd64 microarchitecture variant filter (`["v1"]`, `["v2", "v3"]`, etc.),
    /// set via the `amd64_variant:` key. When set, only amd64 binaries with
    /// `amd64_variant` matching one of the listed values are included. The
    /// legacy `goamd64:` spelling is accepted via serde alias for back-compat
    /// with imported configs. When unset, all amd64 variants are included (no
    /// filtering).
    /// Each entry is typed as [`Amd64Variant`], so any value outside
    /// `v1`..`v4` is rejected when the config is parsed.
    #[serde(alias = "goamd64")]
    pub amd64_variant: Option<Vec<Amd64Variant>>,
    /// Package epoch for versioning (integer as string).
    pub epoch: Option<String>,
    /// Package release number.
    pub release: Option<String>,
    /// Prerelease version suffix.
    pub prerelease: Option<String>,
    /// Version metadata (e.g. git commit hash).
    pub version_metadata: Option<String>,
    /// Package section (e.g. "utils", "devel").
    pub section: Option<String>,
    /// Package priority (e.g. "optional", "required").
    pub priority: Option<String>,
    /// Whether this is a meta-package (no files, only dependencies).
    pub meta: Option<bool>,
    /// File permission umask. Accepts a YAML int (`18`), an octal-prefixed
    /// string (`"0o022"`), or a leading-zero octal string (`"022"`).
    pub umask: Option<StringOrU32>,
    /// Default modification time for files in the package.
    pub mtime: Option<String>,
    /// RPM-specific configuration.
    pub rpm: Option<NfpmRpmConfig>,
    /// Deb-specific configuration.
    pub deb: Option<NfpmDebConfig>,
    /// APK-specific configuration.
    pub apk: Option<NfpmApkConfig>,
    /// Archlinux-specific configuration.
    pub archlinux: Option<NfpmArchlinuxConfig>,
    /// IPK-specific configuration (OpenWrt packages).
    pub ipk: Option<NfpmIpkConfig>,
    /// MSIX-specific configuration (Windows app packages).
    ///
    /// Only consumed when `formats` includes `msix`. nfpm requires
    /// `publisher`, `properties.logo`, and at least one `applications` entry;
    /// everything else has derived defaults.
    ///
    /// ```yaml
    /// msix:
    ///   publisher: "CN=My Company, O=My Company, C=US"
    ///   properties:
    ///     logo: assets/logo.png
    ///   applications:
    ///     - id: MyApp
    ///       executable: myapp.exe
    /// ```
    pub msix: Option<NfpmMsixConfig>,
    /// CGo library installation directories (header, carchive, cshared).
    pub libdirs: Option<NfpmLibdirs>,
    /// Path to a YAML-format changelog file for deb/rpm packages.
    pub changelog: Option<String>,
    /// Template-conditional: skip this nfpm config if rendered result is "false" or empty.
    /// Conditional-skip gate.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Extra file contents whose source files are Tera-rendered before packaging.
    /// Each entry mirrors `contents`; the difference is that at stage time the file at `src` is
    /// read, rendered through the template engine, written to a temp file, and then included
    /// in the package at `dst` using the temp file as the real source. Useful for shipping
    /// config files with templated values (version, commit, maintainer, etc.).
    pub templated_contents: Option<Vec<NfpmContent>>,
    /// Lifecycle scripts whose script-file bodies are Tera-rendered before packaging
    /// Each path is read, rendered through the template engine, written to
    /// a temp file, and used as the real script. If a field is set on both `scripts` and
    /// `templated_scripts`, the templated version wins.
    pub templated_scripts: Option<NfpmScripts>,
}

/// Installation directories for CGo library outputs.
///
/// Controls where header files, static archives, and shared libraries
/// are installed in the package.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmLibdirs {
    /// Installation directory for C header files.
    pub header: Option<String>,
    /// Installation directory for carchive (.a) static libraries.
    pub carchive: Option<String>,
    /// Installation directory for cshared (.so / .dylib) shared libraries.
    pub cshared: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmScripts {
    /// Path to script run before package installation.
    pub preinstall: Option<String>,
    /// Path to script run after package installation.
    pub postinstall: Option<String>,
    /// Path to script run before package removal.
    pub preremove: Option<String>,
    /// Path to script run after package removal.
    pub postremove: Option<String>,
}

/// Backward-compatible alias — nFPM contents share the same `FileInfo` struct.
pub type NfpmFileInfo = FileInfo;

/// A single file/directory entry in an nFPM (or SRPM) package's `contents`
/// list. Merged the formerly-separate `NfpmContentConfig`
/// (used for SRPM) into this struct — `source` / `destination` / `type` are
/// accepted as aliases for `src` / `dst` / the renamed `type` so srpm-style
/// keys still parse.
///
/// `Default` is intentionally **not** derived because `src` and `dst` are
/// required fields with no meaningful defaults — forcing callers to provide
/// them explicitly prevents accidentally packaging empty paths.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NfpmContent {
    /// Source path on the build machine (supports glob patterns and templates).
    ///
    /// Paths are resolved relative to the project root. `..` segments are
    /// NOT stripped, so a templated value resolving to `../../etc/passwd`
    /// will reach outside the project tree — avoid splicing untrusted
    /// template inputs (e.g. arbitrary `{{ Env.X }}` values) into `src`.
    pub src: String,
    /// Destination path inside the package (absolute path, supports templates).
    ///
    /// Same caveat as `src`: `..` segments are passed through to nfpm
    /// verbatim. Templated values from untrusted sources should be
    /// canonicalised by the caller before use.
    pub dst: String,
    /// Content entry type: "config", "config|noreplace", "doc", "dir", "symlink", "ghost", or empty for regular file.
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    /// File ownership and permission metadata.
    pub file_info: Option<NfpmFileInfo>,
    /// Per-packager filter: only include this content entry for the specified packager
    /// (e.g. "deb", "rpm", "apk").
    pub packager: Option<String>,
    /// When true, expand template variables in the `src` and `dst` paths.
    pub expand: Option<bool>,
}

// ---------------------------------------------------------------------------
// nFPM format-specific configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmRpmConfig {
    /// One-line package summary (RPM Summary tag).
    pub summary: Option<String>,
    /// RPM compression algorithm (e.g. "lzma", "gzip", "xz", "zstd").
    pub compression: Option<String>,
    /// RPM group classification (e.g. "System/Tools").
    pub group: Option<String>,
    /// RPM packager identity (e.g. "Build Team <build@example.com>").
    pub packager: Option<String>,
    /// Relocatable RPM prefix paths (e.g. ["/usr", "/etc"]).
    pub prefixes: Option<Vec<String>>,
    /// RPM signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// RPM-specific lifecycle scripts (pretrans/posttrans).
    pub scripts: Option<NfpmRpmScripts>,
    /// RPM BuildHost tag value.
    pub build_host: Option<String>,
}

/// RPM-specific transaction scripts that run outside the normal install/remove lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmRpmScripts {
    /// Script to run before the RPM transaction begins.
    pub pretrans: Option<String>,
    /// Script to run after the RPM transaction completes.
    pub posttrans: Option<String>,
}

impl NfpmRpmConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.compression.is_none()
            && self.group.is_none()
            && self.packager.is_none()
            && self.prefixes.is_none()
            && self.signature.is_none()
            && self.scripts.is_none()
            && self.build_host.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmDebConfig {
    /// Deb compression algorithm (e.g. "gzip", "xz", "zstd", "none").
    pub compression: Option<String>,
    /// Pre-dependency packages (stronger than Depends).
    pub predepends: Option<Vec<String>>,
    /// Deb trigger definitions.
    pub triggers: Option<NfpmDebTriggers>,
    /// Packages this package breaks (Breaks relationship).
    pub breaks: Option<Vec<String>>,
    /// Lintian overrides to embed in the package.
    pub lintian_overrides: Option<Vec<String>>,
    /// Deb signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// Additional control fields (e.g. Bugs, Built-Using).
    pub fields: Option<HashMap<String, String>>,
    /// Deb-specific maintainer scripts (rules, templates, config).
    pub scripts: Option<NfpmDebScripts>,
    /// Target architecture variant in deb nomenclature (e.g. `amd64v3`).
    ///
    /// Auto-derived from the built binary's `amd64_variant` (`v1`..`v4`) GOAMD64
    /// microarchitecture metadata when unset, so an amd64 deb is tagged with the
    /// microarch it was compiled for. Maps to nfpm's `deb.arch_variant`.
    pub arch_variant: Option<String>,
}

/// Deb-specific maintainer scripts for package configuration and rules.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmDebScripts {
    /// Path to debian/rules file.
    pub rules: Option<String>,
    /// Path to debian/templates file (debconf templates).
    pub templates: Option<String>,
    /// Path to debian/config script (debconf configuration).
    pub config: Option<String>,
}

impl NfpmDebConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.compression.is_none()
            && self.predepends.is_none()
            && self.triggers.is_none()
            && self.breaks.is_none()
            && self.lintian_overrides.is_none()
            && self.signature.is_none()
            && self.fields.is_none()
            && self.scripts.is_none()
            // `arch_variant` keeps the deb block alive when set alone: a config
            // carrying only `arch_variant: v3` must not be dropped as "empty",
            // which would silently lose the microarch tag (`amd64v3` → plain
            // `amd64`).
            && self.arch_variant.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmDebTriggers {
    /// Deb interest triggers: package waits for these triggers to complete.
    pub interest: Option<Vec<String>>,
    /// Deb interest-await triggers: package waits with synchronous trigger processing.
    pub interest_await: Option<Vec<String>>,
    /// Deb interest-noawait triggers: package registers interest without waiting.
    pub interest_noawait: Option<Vec<String>>,
    /// Deb activate triggers: package activates these triggers after install.
    pub activate: Option<Vec<String>>,
    /// Deb activate-await triggers: activate and wait for synchronous trigger processing.
    pub activate_await: Option<Vec<String>>,
    /// Deb activate-noawait triggers: activate without waiting.
    pub activate_noawait: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmApkConfig {
    /// APK signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// APK-specific lifecycle scripts (preupgrade/postupgrade).
    pub scripts: Option<NfpmApkScripts>,
}

/// APK-specific upgrade lifecycle scripts.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmApkScripts {
    /// Script to run before upgrading an existing package.
    pub preupgrade: Option<String>,
    /// Script to run after upgrading an existing package.
    pub postupgrade: Option<String>,
}

impl NfpmApkConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.signature.is_none() && self.scripts.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmArchlinuxConfig {
    /// Base package name for split packages.
    pub pkgbase: Option<String>,
    /// Packager identity (e.g. "Build Team <build@example.com>").
    pub packager: Option<String>,
    /// Archlinux-specific lifecycle scripts.
    pub scripts: Option<NfpmArchlinuxScripts>,
}

impl NfpmArchlinuxConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.pkgbase.is_none() && self.packager.is_none() && self.scripts.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmArchlinuxScripts {
    /// Script to run before upgrading an existing package.
    pub preupgrade: Option<String>,
    /// Script to run after upgrading an existing package.
    pub postupgrade: Option<String>,
}

/// IPK (OpenWrt) package-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmIpkConfig {
    /// ABI version string for the package.
    pub abi_version: Option<String>,
    /// Alternative file links managed by the update-alternatives system.
    pub alternatives: Option<Vec<NfpmIpkAlternative>>,
    /// Whether the package was automatically installed as a dependency.
    pub auto_installed: Option<bool>,
    /// Whether the package is essential for the system.
    pub essential: Option<bool>,
    /// Strong pre-dependencies that must be fully installed before this package.
    pub predepends: Option<Vec<String>>,
    /// Tags for categorizing the package.
    pub tags: Option<Vec<String>>,
    /// Additional control fields as key-value pairs.
    pub fields: Option<HashMap<String, String>>,
}

impl NfpmIpkConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.abi_version.is_none()
            && self.alternatives.is_none()
            && self.auto_installed.is_none()
            && self.essential.is_none()
            && self.predepends.is_none()
            && self.tags.is_none()
            && self.fields.is_none()
    }
}

/// MSIX (Windows app package) specific configuration.
///
/// Field names mirror nfpm's `msix:` YAML block. nfpm validates that
/// `publisher`, `properties.logo`, and at least one application (with `id`
/// and `executable`) are set; the remaining fields have derived defaults
/// (e.g. `entry_point` defaults to `Windows.FullTrustApplication`, display
/// names default to the package name, and `dependencies` defaults to a
/// `Windows.Desktop` 10.0.17763.0–10.0.22621.0 target device family).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixConfig {
    /// Target architecture override in MSIX nomenclature (`x64`, `x86`,
    /// `arm64`, `arm`, `neutral`). When unset, derived from the build target
    /// (e.g. `x86_64` → `x64`).
    pub arch: Option<String>,
    /// Publisher identity, matching the signing certificate subject
    /// (e.g. `"CN=My Company, O=My Company, C=US"`). Required by nfpm.
    /// Templated.
    pub publisher: Option<String>,
    /// Package identity fields.
    pub identity: Option<NfpmMsixIdentity>,
    /// Package display properties.
    pub properties: Option<NfpmMsixProperties>,
    /// Applications contained in the package (nfpm requires at least one,
    /// each with `id` and `executable`). When omitted, anodizer derives one
    /// application per packaged binary — `executable` is the binary's file
    /// name and `id` its sanitized file stem — so this only needs setting to
    /// override entry points or visual elements.
    pub applications: Option<Vec<NfpmMsixApplication>>,
    /// Target device family dependencies. Defaults to
    /// `Windows.Desktop` min `10.0.17763.0` / max tested `10.0.22621.0`.
    pub dependencies: Option<NfpmMsixDependencies>,
    /// Capability declarations for the package.
    pub capabilities: Option<NfpmMsixCapabilities>,
    /// MSIX signing configuration.
    pub signature: Option<NfpmMsixSignature>,
}

impl NfpmMsixConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.arch.is_none()
            && self.publisher.is_none()
            && self.identity.is_none()
            && self.properties.is_none()
            && self.applications.is_none()
            && self.dependencies.is_none()
            && self.capabilities.is_none()
            && self.signature.is_none()
    }
}

/// Identity fields for MSIX packages.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixIdentity {
    /// Resource identifier for the package identity (e.g. `"en-us"`).
    pub resource_id: Option<String>,
}

/// Display properties for MSIX packages.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixProperties {
    /// Package display name (defaults to the package name). Templated.
    pub display_name: Option<String>,
    /// Publisher display name (defaults to the package name). Templated.
    pub publisher_display_name: Option<String>,
    /// Path to the package logo image (e.g. `assets/logo.png`). Required by
    /// nfpm. Templated.
    pub logo: Option<String>,
}

/// An application entry in an MSIX package.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixApplication {
    /// Application identifier (e.g. `"MyApp"`). Required by nfpm.
    pub id: Option<String>,
    /// Executable path inside the package (e.g. `myapp.exe`). Required by nfpm.
    pub executable: Option<String>,
    /// Application entry point (default: `Windows.FullTrustApplication`).
    pub entry_point: Option<String>,
    /// Visual presentation settings for this application.
    pub visual_elements: Option<NfpmMsixVisualElements>,
}

/// Visual presentation settings for an MSIX application.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixVisualElements {
    /// Application display name (defaults to the package name).
    pub display_name: Option<String>,
    /// Application description (defaults to the package description).
    pub description: Option<String>,
    /// Tile background color (default: `transparent`).
    pub background_color: Option<String>,
    /// Path to the 150x150 tile logo (defaults to `properties.logo`).
    pub square150x150_logo: Option<String>,
    /// Path to the 44x44 tile logo (defaults to `properties.logo`).
    pub square44x44_logo: Option<String>,
}

/// Dependency information for MSIX packages.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixDependencies {
    /// Target device families the package supports.
    pub target_device_families: Option<Vec<NfpmMsixTargetDeviceFamily>>,
}

/// A target device family for an MSIX package.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixTargetDeviceFamily {
    /// Device family name (e.g. `"Windows.Desktop"`).
    pub name: Option<String>,
    /// Minimum OS version (e.g. `"10.0.17763.0"`).
    pub min_version: Option<String>,
    /// Maximum tested OS version (e.g. `"10.0.22621.0"`).
    pub max_version_tested: Option<String>,
}

/// Capability declarations for MSIX packages.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixCapabilities {
    /// General capabilities (e.g. `["internetClient"]`).
    pub capabilities: Option<Vec<String>>,
    /// Device capabilities (e.g. `["microphone"]`).
    pub device_capabilities: Option<Vec<String>>,
    /// Restricted capabilities (e.g. `["runFullTrust"]` — added
    /// automatically when an application uses the full-trust entry point).
    pub restricted: Option<Vec<String>>,
}

/// Signing configuration for MSIX packages.
///
/// The passphrase is NOT a config field: nfpm reads it from the
/// `NFPM_MSIX_PASSPHRASE` environment variable, which anodizer resolves via
/// the same `NFPM_{ID}_MSIX_PASSPHRASE` → `NFPM_{ID}_PASSPHRASE` →
/// `NFPM_PASSPHRASE` fallback the other signature blocks use and forwards to
/// the nfpm subprocess.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmMsixSignature {
    /// Path to the PFX certificate file used to sign the package. Templated.
    pub pfx_file: Option<String>,
}

/// An alternative file link for IPK's update-alternatives system.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmIpkAlternative {
    /// Priority for alternative selection (higher wins).
    pub priority: Option<i32>,
    /// Target file path that the alternative points to.
    pub target: Option<String>,
    /// Symlink name in the alternatives directory.
    pub link_name: Option<String>,
}

#[cfg(test)]
mod is_empty_tests {
    use super::*;

    /// `arch_variant` is the load-bearing single field that, when set in
    /// isolation, must keep the deb block alive — otherwise the "drop empty
    /// blocks" path silently dropped microarch tagging (`amd64v3` collapsing to
    /// plain `amd64`).
    #[test]
    fn deb_arch_variant_alone_is_not_empty() {
        let cfg = NfpmDebConfig {
            arch_variant: Some("v3".to_string()),
            ..Default::default()
        };
        assert!(
            !cfg.is_empty(),
            "deb block with only arch_variant must NOT be dropped"
        );
    }

    /// Sanity: a fully empty deb block IS empty.
    #[test]
    fn deb_default_is_empty() {
        assert!(NfpmDebConfig::default().is_empty());
    }

    /// The legacy `goamd64:` spelling folds into `amd64_variant` so imported
    /// configs keep parsing under `deny_unknown_fields`.
    #[test]
    fn nfpm_goamd64_alias_parses_into_amd64_variant() {
        let nfpm: NfpmConfig =
            serde_yaml_ng::from_str("formats: [deb]\ngoamd64: [v2, v3]").unwrap();
        assert_eq!(
            nfpm.amd64_variant.as_deref(),
            Some([Amd64Variant::V2, Amd64Variant::V3].as_slice())
        );
    }

    #[test]
    fn nfpm_canonical_amd64_variant_still_parses() {
        let nfpm: NfpmConfig =
            serde_yaml_ng::from_str("formats: [deb]\namd64_variant: [v1]").unwrap();
        assert_eq!(
            nfpm.amd64_variant.as_deref(),
            Some([Amd64Variant::V1].as_slice())
        );
    }

    /// A typo'd level in the list form must fail at parse — the field is a
    /// closed enum, not free text, so `x86-64-v3` cannot silently disable
    /// the variant filter.
    #[test]
    fn nfpm_amd64_variant_list_rejects_garbage_at_parse() {
        let err =
            serde_yaml_ng::from_str::<NfpmConfig>("formats: [deb]\namd64_variant: [v2, x86-64-v3]")
                .unwrap_err()
                .to_string();
        assert!(
            err.contains("unknown variant `x86-64-v3`")
                && err.contains("expected one of `v1`, `v2`, `v3`, `v4`"),
            "parse error must name the bad value and the valid set: {err}"
        );
    }
}

/// Unified signature configuration shared by nFPM (deb/rpm/apk) and SRPM
/// packages — SRPM's surface is a strict subset, so a single struct covers
/// both. The legacy SRPM `passphrase:` key is accepted as a serde alias
/// for `key_passphrase:` so both spellings parse.
///
/// There are three distinct signature types (`NFPMRPMSignature`,
/// `NFPMDebSignature`, `NFPMAPKSignature`) with overlapping but slightly
/// different fields. Anodizer's union here avoids the 3-struct cascade
/// when 90% of fields overlap.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NfpmSignatureConfig {
    /// Path to the signing key file.
    pub key_file: Option<String>,
    /// Key ID to use for signing.
    pub key_id: Option<String>,
    /// Passphrase for the signing key. Falls back to `NFPM_PASSPHRASE` /
    /// `SRPM_PASSPHRASE` env vars in their respective stages.
    pub key_passphrase: Option<String>,
    /// Public key name for APK signatures (defaults to `<maintainer email>.rsa.pub`).
    pub key_name: Option<String>,
    /// Signature type for deb packages: "origin", "maint", or "archive" (default: "origin").
    #[serde(rename = "type")]
    pub type_: Option<String>,
}
