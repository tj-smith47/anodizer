use std::collections::HashMap;
use std::path::PathBuf;

use colored::Colorize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtifactKind {
    // --- Build outputs ---
    Binary,
    /// Binary marked for upload (checksummed, signed, released).
    /// Distinct from Binary which is a raw build output.
    UploadableBinary,
    UniversalBinary,
    Library,
    Header,
    CArchive,
    CShared,
    Wasm,

    // --- Python packages ---
    PyWheel,
    PySdist,

    // --- Packaged archives ---
    Archive,
    SourceArchive,
    Makeself,

    // --- Linux packages ---
    LinuxPackage,
    Snap,
    PublishableSnapcraft,
    Flatpak,
    SourceRpm,

    // --- macOS/Windows installers ---
    DiskImage,
    Installer,
    MacOsPackage,

    // --- Container images ---
    DockerImage,
    DockerImageV2,
    PublishableDockerImage,
    DockerManifest,
    DockerDigest,

    // --- Publisher manifests ---
    BrewFormula,
    BrewCask,
    Nixpkg,
    ScoopManifest,
    PublishableChocolatey,
    WingetInstaller,
    WingetDefaultLocale,
    WingetVersion,
    PkgBuild,
    SrcInfo,
    SourcePkgBuild,
    SourceSrcInfo,
    KrewPluginManifest,

    // --- Integrity/metadata ---
    Checksum,
    Signature,
    Certificate,
    Sbom,
    Metadata,
    UploadableFile,
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ArtifactKind {
    /// Return the snake_case string representation (matching serde serialization).
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Binary => "binary",
            ArtifactKind::UploadableBinary => "uploadable_binary",
            ArtifactKind::UniversalBinary => "universal_binary",
            ArtifactKind::Library => "library",
            ArtifactKind::Header => "header",
            ArtifactKind::CArchive => "c_archive",
            ArtifactKind::CShared => "c_shared",
            ArtifactKind::Wasm => "wasm",
            ArtifactKind::PyWheel => "py_wheel",
            ArtifactKind::PySdist => "py_sdist",
            ArtifactKind::Archive => "archive",
            ArtifactKind::SourceArchive => "source_archive",
            ArtifactKind::Makeself => "makeself",
            ArtifactKind::LinuxPackage => "linux_package",
            ArtifactKind::Snap => "snap",
            ArtifactKind::PublishableSnapcraft => "publishable_snapcraft",
            ArtifactKind::Flatpak => "flatpak",
            ArtifactKind::SourceRpm => "source_rpm",
            ArtifactKind::DiskImage => "disk_image",
            ArtifactKind::Installer => "installer",
            ArtifactKind::MacOsPackage => "macos_package",
            ArtifactKind::DockerImage => "docker_image",
            ArtifactKind::DockerImageV2 => "docker_image_v2",
            ArtifactKind::PublishableDockerImage => "publishable_docker_image",
            ArtifactKind::DockerManifest => "docker_manifest",
            ArtifactKind::DockerDigest => "docker_digest",
            ArtifactKind::BrewFormula => "brew_formula",
            ArtifactKind::BrewCask => "brew_cask",
            ArtifactKind::Nixpkg => "nixpkg",
            ArtifactKind::ScoopManifest => "scoop_manifest",
            ArtifactKind::PublishableChocolatey => "publishable_chocolatey",
            ArtifactKind::WingetInstaller => "winget_installer",
            ArtifactKind::WingetDefaultLocale => "winget_default_locale",
            ArtifactKind::WingetVersion => "winget_version",
            ArtifactKind::PkgBuild => "pkg_build",
            ArtifactKind::SrcInfo => "src_info",
            ArtifactKind::SourcePkgBuild => "source_pkg_build",
            ArtifactKind::SourceSrcInfo => "source_src_info",
            ArtifactKind::KrewPluginManifest => "krew_plugin_manifest",
            ArtifactKind::Checksum => "checksum",
            ArtifactKind::Signature => "signature",
            ArtifactKind::Certificate => "certificate",
            ArtifactKind::Sbom => "sbom",
            ArtifactKind::Metadata => "metadata",
            ArtifactKind::UploadableFile => "uploadable_file",
        }
    }

    /// Parse a snake_case string into an ArtifactKind.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "binary" => Some(ArtifactKind::Binary),
            "uploadable_binary" => Some(ArtifactKind::UploadableBinary),
            "universal_binary" => Some(ArtifactKind::UniversalBinary),
            "library" => Some(ArtifactKind::Library),
            "header" => Some(ArtifactKind::Header),
            "c_archive" => Some(ArtifactKind::CArchive),
            "c_shared" => Some(ArtifactKind::CShared),
            "wasm" => Some(ArtifactKind::Wasm),
            "py_wheel" => Some(ArtifactKind::PyWheel),
            "py_sdist" => Some(ArtifactKind::PySdist),
            "archive" => Some(ArtifactKind::Archive),
            "source_archive" => Some(ArtifactKind::SourceArchive),
            "makeself" => Some(ArtifactKind::Makeself),
            "linux_package" => Some(ArtifactKind::LinuxPackage),
            "snap" => Some(ArtifactKind::Snap),
            "publishable_snapcraft" => Some(ArtifactKind::PublishableSnapcraft),
            "flatpak" => Some(ArtifactKind::Flatpak),
            "source_rpm" => Some(ArtifactKind::SourceRpm),
            "disk_image" => Some(ArtifactKind::DiskImage),
            "installer" => Some(ArtifactKind::Installer),
            "macos_package" => Some(ArtifactKind::MacOsPackage),
            "docker_image" => Some(ArtifactKind::DockerImage),
            "docker_image_v2" => Some(ArtifactKind::DockerImageV2),
            "publishable_docker_image" => Some(ArtifactKind::PublishableDockerImage),
            "docker_manifest" => Some(ArtifactKind::DockerManifest),
            "docker_digest" => Some(ArtifactKind::DockerDigest),
            "brew_formula" => Some(ArtifactKind::BrewFormula),
            "brew_cask" => Some(ArtifactKind::BrewCask),
            "nixpkg" => Some(ArtifactKind::Nixpkg),
            "scoop_manifest" => Some(ArtifactKind::ScoopManifest),
            "publishable_chocolatey" => Some(ArtifactKind::PublishableChocolatey),
            "winget_installer" => Some(ArtifactKind::WingetInstaller),
            "winget_default_locale" => Some(ArtifactKind::WingetDefaultLocale),
            "winget_version" => Some(ArtifactKind::WingetVersion),
            "pkg_build" => Some(ArtifactKind::PkgBuild),
            "src_info" => Some(ArtifactKind::SrcInfo),
            "source_pkg_build" => Some(ArtifactKind::SourcePkgBuild),
            "source_src_info" => Some(ArtifactKind::SourceSrcInfo),
            "krew_plugin_manifest" => Some(ArtifactKind::KrewPluginManifest),
            "checksum" => Some(ArtifactKind::Checksum),
            "signature" => Some(ArtifactKind::Signature),
            "certificate" => Some(ArtifactKind::Certificate),
            "sbom" => Some(ArtifactKind::Sbom),
            "metadata" => Some(ArtifactKind::Metadata),
            "uploadable_file" => Some(ArtifactKind::UploadableFile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    /// Canonical artifact name, set at add-time from the path's filename (trimmed).
    pub name: String,
    pub target: Option<String>,
    pub crate_name: String,
    pub metadata: HashMap<String, String>,
    /// File size in bytes, populated by report_sizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl Artifact {
    /// Return the artifact filename.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the OS component of the target (e.g., "linux", "darwin", "windows").
    pub fn goos(&self) -> Option<String> {
        self.target.as_ref().map(|t| crate::target::map_target(t).0)
    }

    /// Return the arch component of the target (e.g., "amd64", "arm64").
    pub fn goarch(&self) -> Option<String> {
        self.target.as_ref().map(|t| crate::target::map_target(t).1)
    }

    /// Check if this artifact replaces single-arch variants (universal binary dedup).
    /// GoReleaser parity: `OnlyReplacingUnibins` — when a universal binary has
    /// `replaces=true`, it supersedes the per-arch binaries for publisher consumption.
    /// Artifacts without the `replaces` metadata key default to `true` (included).
    pub fn only_replacing_unibins(&self) -> bool {
        self.metadata.get("replaces").is_none_or(|v| v != "false")
    }

    /// Return the list of extra binary names bundled in this archive artifact.
    pub fn extra_binaries(&self) -> Vec<String> {
        self.metadata
            .get("extra_binaries")
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return the single binary name for an uploadable binary artifact.
    pub fn extra_binary(&self) -> Option<String> {
        self.metadata.get("binary").cloned()
    }
}

#[derive(Debug, Default)]
pub struct ArtifactRegistry {
    artifacts: Vec<Artifact>,
}

impl ArtifactRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, mut artifact: Artifact) {
        // Set canonical name from path filename if the caller hasn't provided one.
        let name = if artifact.name.is_empty() {
            let derived = artifact
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact")
                .trim()
                .to_string();
            artifact.name = derived.clone();
            derived
        } else {
            artifact.name.clone()
        };

        // Normalize path: convert to forward slashes for cross-platform consistency.
        let path_str = artifact.path.to_string_lossy().replace('\\', "/");
        artifact.path = PathBuf::from(path_str);

        // Warn on duplicate names for uploadable artifact types.
        if is_uploadable(artifact.kind)
            && let Some(existing) = self
                .artifacts
                .iter()
                .find(|a| is_uploadable(a.kind) && a.name == name)
        {
            eprintln!(
                "{} artifact '{}' already registered (existing: {}, new: {}); upload may fail with duplicate error",
                "Warning:".yellow().bold(),
                name,
                existing.path.display(),
                artifact.path.display()
            );
        }

        self.artifacts.push(artifact);
    }

    pub fn by_kind(&self, kind: ArtifactKind) -> Vec<&Artifact> {
        self.artifacts.iter().filter(|a| a.kind == kind).collect()
    }

    pub fn by_kind_and_crate(&self, kind: ArtifactKind, crate_name: &str) -> Vec<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.kind == kind && a.crate_name == crate_name)
            .collect()
    }

    pub fn by_kinds_and_crate(&self, kinds: &[ArtifactKind], crate_name: &str) -> Vec<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| kinds.contains(&a.kind) && a.crate_name == crate_name)
            .collect()
    }

    pub fn all(&self) -> &[Artifact] {
        &self.artifacts
    }

    pub fn all_mut(&mut self) -> &mut [Artifact] {
        &mut self.artifacts
    }

    /// Filter artifacts by a predicate, returning matching references.
    pub fn filter<F: Fn(&Artifact) -> bool>(&self, predicate: F) -> Vec<&Artifact> {
        self.artifacts.iter().filter(|a| predicate(a)).collect()
    }

    /// Remove all artifacts whose path matches one of the given paths.
    pub fn remove_by_paths(&mut self, paths: &[std::path::PathBuf]) {
        self.artifacts.retain(|a| !paths.contains(&a.path));
    }

    /// Serialize all artifacts to a JSON value suitable for writing to artifacts.json.
    /// Normalizes all artifact paths to use forward slashes for cross-platform
    /// consistency (GoReleaser always writes forward slashes).
    pub fn to_artifacts_json(&self) -> anyhow::Result<serde_json::Value> {
        let mut val = serde_json::to_value(&self.artifacts)?;
        // Normalize backslashes in path fields to forward slashes.
        if let Some(arr) = val.as_array_mut() {
            for entry in arr {
                if let Some(path) = entry
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|s| s.replace('\\', "/"))
                {
                    entry["path"] = serde_json::Value::String(path);
                }
            }
        }
        Ok(val)
    }
}

/// Artifact kinds that should be included in size reporting.
/// Matches GoReleaser's reportsizes filter: all uploadable types (including
/// UploadableBinary, Makeself, PyWheel, PySdist) + build outputs (Binary,
/// UniversalBinary, Library, Header, CArchive, CShared, Wasm) + Snap.
pub fn size_reportable_kinds() -> &'static [ArtifactKind] {
    &[
        // Uploadable types (all appear in releases)
        ArtifactKind::Archive,
        ArtifactKind::SourceArchive,
        ArtifactKind::UploadableFile,
        ArtifactKind::Makeself,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Sbom,
        ArtifactKind::PyWheel,
        ArtifactKind::PySdist,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
        ArtifactKind::DiskImage,
        ArtifactKind::Installer,
        ArtifactKind::MacOsPackage,
        ArtifactKind::Snap,
        ArtifactKind::PublishableSnapcraft,
        // Build outputs (GoReleaser reports Binary, CArchive, CShared, Header)
        ArtifactKind::Binary,
        ArtifactKind::UploadableBinary,
        ArtifactKind::UniversalBinary,
        ArtifactKind::Library,
        ArtifactKind::Header,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
        ArtifactKind::Wasm,
    ]
}

/// Artifact kinds that are uploadable to releases/blob storage.
/// Matches GoReleaser's ReleaseUploadableTypes — the canonical list of types
/// that should be uploaded, checksummed, signed, and distributed.
pub fn uploadable_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
        ArtifactKind::SourceArchive,
        ArtifactKind::UploadableFile,
        ArtifactKind::Makeself,
        ArtifactKind::LinuxPackage,
        ArtifactKind::PublishableSnapcraft,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Sbom,
        ArtifactKind::PyWheel,
        ArtifactKind::PySdist,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
        ArtifactKind::DiskImage,
        ArtifactKind::Installer,
        ArtifactKind::MacOsPackage,
    ]
}

/// Artifact kinds eligible for release upload, matching GoReleaser's
/// `ReleaseUploadableTypes` ordering from `internal/pipe/release/release.go`.
/// This is the canonical list used by the GitHub release publisher and by
/// blob storage when deciding which artifacts to include.
///
/// Kept intentionally narrower than [`uploadable_kinds`] — for example,
/// [`ArtifactKind::PublishableSnapcraft`] appears in anodize's internal
/// uploadable list but is not released to GitHub in GoReleaser.
pub fn release_uploadable_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
        ArtifactKind::UploadableFile,
        ArtifactKind::SourceArchive,
        ArtifactKind::Makeself,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Sbom,
        ArtifactKind::PyWheel,
        ArtifactKind::PySdist,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
    ]
}

/// Check if an artifact kind is uploadable.
fn is_uploadable(kind: ArtifactKind) -> bool {
    uploadable_kinds().contains(&kind)
}

/// Format a byte count into a human-readable string (e.g. "4.2 MB").
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Populate artifact sizes and print a formatted size table.
///
/// Filters artifacts to [`size_reportable_kinds`] (matching GoReleaser's
/// `reportsizes` pipe), stores the file size in each artifact's `size` field,
/// and prints a human-readable table.
pub fn print_size_report(registry: &mut ArtifactRegistry, log: &crate::log::StageLogger) {
    let reportable = size_reportable_kinds();
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut total: u64 = 0;

    for artifact in registry.all_mut() {
        if !reportable.contains(&artifact.kind) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&artifact.path) {
            let size = meta.len();
            artifact.size = Some(size);
            let name = artifact
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| artifact.path.display().to_string());
            entries.push((name, size));
            total += size;
        }
    }

    if entries.is_empty() {
        return;
    }

    let max_name_len = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    log.status("");
    log.status("Artifact Sizes:");
    for (name, size) in &entries {
        log.status(&format!(
            "  {:<width$}  {}",
            name,
            format_size(*size),
            width = max_name_len
        ));
    }
    log.status(&format!(
        "  {:<width$}  {}",
        "Total:",
        format_size(total),
        width = max_name_len
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_add_and_query_artifacts() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/cfgd"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/cfgd.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let binaries = registry.by_kind(ArtifactKind::Binary);
        assert_eq!(binaries.len(), 1);

        let archives = registry.by_kind_and_crate(ArtifactKind::Archive, "cfgd");
        assert_eq!(archives.len(), 1);
    }

    #[test]
    fn test_empty_query() {
        let registry = ArtifactRegistry::new();
        assert!(registry.by_kind(ArtifactKind::Binary).is_empty());
    }

    #[test]
    fn test_by_kinds_and_crate() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "bin".to_string(),
            path: PathBuf::from("bin"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::UniversalBinary,
            name: "ubin".to_string(),
            path: PathBuf::from("ubin"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Header,
            name: "hdr".to_string(),
            path: PathBuf::from("hdr"),
            target: None,
            crate_name: "other".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let results = registry.by_kinds_and_crate(
            &[ArtifactKind::Binary, ArtifactKind::UniversalBinary],
            "app",
        );
        assert_eq!(results.len(), 2);

        // Header belongs to "other" crate, not "app"
        let results = registry.by_kinds_and_crate(&[ArtifactKind::Header], "app");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_to_artifacts_json_empty() {
        let registry = ArtifactRegistry::new();
        let json = registry.to_artifacts_json().unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_to_artifacts_json_with_artifacts() {
        let mut registry = ArtifactRegistry::new();
        let mut meta = HashMap::new();
        meta.insert("format".to_string(), "tar.gz".to_string());
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta,
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_checksums.txt"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // First artifact
        let first = &arr[0];
        assert_eq!(first["kind"], "archive");
        assert_eq!(first["path"], "dist/myapp-1.0.0-linux-amd64.tar.gz");
        assert_eq!(first["target"], "x86_64-unknown-linux-gnu");
        assert_eq!(first["crate_name"], "myapp");
        assert_eq!(first["metadata"]["format"], "tar.gz");

        // Second artifact
        let second = &arr[1];
        assert_eq!(second["kind"], "checksum");
        assert!(second["target"].is_null());
    }

    #[test]
    fn test_metadata_json_is_valid_json_string() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let serialized = serde_json::to_string_pretty(&json).unwrap();
        // Should be parseable back
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, json);
    }

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10240), "10.0 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(4404019), "4.2 MB");
    }

    #[test]
    fn test_format_size_gigabytes() {
        assert_eq!(format_size(1073741824), "1.0 GB");
        assert_eq!(format_size(2147483648), "2.0 GB");
    }

    #[test]
    fn test_artifact_kind_serializes_to_snake_case() {
        let json = serde_json::to_value(ArtifactKind::DockerImage).unwrap();
        assert_eq!(json, "docker_image");
        let json = serde_json::to_value(ArtifactKind::LinuxPackage).unwrap();
        assert_eq!(json, "linux_package");
        let json = serde_json::to_value(ArtifactKind::Binary).unwrap();
        assert_eq!(json, "binary");
    }

    #[test]
    fn test_artifact_kind_new_variants_serialize() {
        assert_eq!(
            serde_json::to_value(ArtifactKind::UploadableBinary).unwrap(),
            "uploadable_binary"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::UniversalBinary).unwrap(),
            "universal_binary"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Header).unwrap(),
            "header"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::CArchive).unwrap(),
            "c_archive"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::CShared).unwrap(),
            "c_shared"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PyWheel).unwrap(),
            "py_wheel"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PySdist).unwrap(),
            "py_sdist"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Makeself).unwrap(),
            "makeself"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::DockerImageV2).unwrap(),
            "docker_image_v2"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableDockerImage).unwrap(),
            "publishable_docker_image"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableSnapcraft).unwrap(),
            "publishable_snapcraft"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourceRpm).unwrap(),
            "source_rpm"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::BrewFormula).unwrap(),
            "brew_formula"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::BrewCask).unwrap(),
            "brew_cask"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Nixpkg).unwrap(),
            "nixpkg"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::ScoopManifest).unwrap(),
            "scoop_manifest"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableChocolatey).unwrap(),
            "publishable_chocolatey"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetInstaller).unwrap(),
            "winget_installer"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetDefaultLocale).unwrap(),
            "winget_default_locale"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetVersion).unwrap(),
            "winget_version"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PkgBuild).unwrap(),
            "pkg_build"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SrcInfo).unwrap(),
            "src_info"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourcePkgBuild).unwrap(),
            "source_pkg_build"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourceSrcInfo).unwrap(),
            "source_src_info"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::KrewPluginManifest).unwrap(),
            "krew_plugin_manifest"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::UploadableFile).unwrap(),
            "uploadable_file"
        );
    }

    #[test]
    fn test_artifact_kind_library_and_wasm() {
        let json = serde_json::to_value(ArtifactKind::Library).unwrap();
        assert_eq!(json, "library");
        let json = serde_json::to_value(ArtifactKind::Wasm).unwrap();
        assert_eq!(json, "wasm");
    }

    #[test]
    fn test_artifact_kind_as_str_library_wasm() {
        assert_eq!(ArtifactKind::Library.as_str(), "library");
        assert_eq!(ArtifactKind::Wasm.as_str(), "wasm");
    }

    #[test]
    fn test_artifact_kind_parse_roundtrip_all_variants() {
        let all_variants = [
            ArtifactKind::Binary,
            ArtifactKind::UploadableBinary,
            ArtifactKind::UniversalBinary,
            ArtifactKind::Library,
            ArtifactKind::Header,
            ArtifactKind::CArchive,
            ArtifactKind::CShared,
            ArtifactKind::Wasm,
            ArtifactKind::PyWheel,
            ArtifactKind::PySdist,
            ArtifactKind::Archive,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Snap,
            ArtifactKind::PublishableSnapcraft,
            ArtifactKind::Flatpak,
            ArtifactKind::SourceRpm,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
            ArtifactKind::DockerImage,
            ArtifactKind::DockerImageV2,
            ArtifactKind::PublishableDockerImage,
            ArtifactKind::DockerManifest,
            ArtifactKind::BrewFormula,
            ArtifactKind::BrewCask,
            ArtifactKind::Nixpkg,
            ArtifactKind::ScoopManifest,
            ArtifactKind::PublishableChocolatey,
            ArtifactKind::WingetInstaller,
            ArtifactKind::WingetDefaultLocale,
            ArtifactKind::WingetVersion,
            ArtifactKind::PkgBuild,
            ArtifactKind::SrcInfo,
            ArtifactKind::SourcePkgBuild,
            ArtifactKind::SourceSrcInfo,
            ArtifactKind::KrewPluginManifest,
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Certificate,
            ArtifactKind::Sbom,
            ArtifactKind::Metadata,
            ArtifactKind::UploadableFile,
        ];
        for variant in &all_variants {
            let s = variant.as_str();
            let parsed =
                ArtifactKind::parse(s).unwrap_or_else(|| panic!("parse({:?}) returned None", s));
            assert_eq!(*variant, parsed, "roundtrip failed for {:?}", s);
        }
        assert_eq!(all_variants.len(), 44, "update test when adding variants");
    }

    #[test]
    fn test_query_by_library_and_wasm_kinds() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Library,
            name: String::new(),
            path: PathBuf::from("target/libmylib.so"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Wasm,
            name: String::new(),
            path: PathBuf::from("target/mylib.wasm"),
            target: Some("wasm32-unknown-unknown".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
            size: None,
        });

        assert_eq!(registry.by_kind(ArtifactKind::Library).len(), 1);
        assert_eq!(registry.by_kind(ArtifactKind::Wasm).len(), 1);
        assert_eq!(
            registry
                .by_kind_and_crate(ArtifactKind::Wasm, "mylib")
                .len(),
            1
        );
    }

    #[test]
    fn test_size_reportable_kinds_includes_releasable_and_binaries() {
        let kinds = size_reportable_kinds();
        // Uploadable types
        assert!(kinds.contains(&ArtifactKind::Archive));
        assert!(kinds.contains(&ArtifactKind::SourceArchive));
        assert!(kinds.contains(&ArtifactKind::UploadableFile));
        assert!(kinds.contains(&ArtifactKind::Makeself));
        assert!(kinds.contains(&ArtifactKind::LinuxPackage));
        assert!(kinds.contains(&ArtifactKind::Flatpak));
        assert!(kinds.contains(&ArtifactKind::SourceRpm));
        assert!(kinds.contains(&ArtifactKind::PyWheel));
        assert!(kinds.contains(&ArtifactKind::PySdist));
        assert!(kinds.contains(&ArtifactKind::Sbom));
        assert!(kinds.contains(&ArtifactKind::Checksum));
        assert!(kinds.contains(&ArtifactKind::Signature));
        assert!(kinds.contains(&ArtifactKind::Certificate));
        assert!(kinds.contains(&ArtifactKind::DiskImage));
        assert!(kinds.contains(&ArtifactKind::Installer));
        assert!(kinds.contains(&ArtifactKind::MacOsPackage));
        assert!(kinds.contains(&ArtifactKind::Snap));
        // Build outputs
        assert!(kinds.contains(&ArtifactKind::Binary));
        assert!(kinds.contains(&ArtifactKind::UniversalBinary));
        assert!(kinds.contains(&ArtifactKind::Library));
        assert!(kinds.contains(&ArtifactKind::Header));
        assert!(kinds.contains(&ArtifactKind::CArchive));
        assert!(kinds.contains(&ArtifactKind::CShared));
        assert!(kinds.contains(&ArtifactKind::Wasm));
    }

    #[test]
    fn test_size_reportable_kinds_excludes_non_releasable() {
        let kinds = size_reportable_kinds();
        assert!(!kinds.contains(&ArtifactKind::DockerImage));
        assert!(!kinds.contains(&ArtifactKind::DockerManifest));
        assert!(!kinds.contains(&ArtifactKind::Metadata));
        assert!(!kinds.contains(&ArtifactKind::BrewFormula));
        assert!(!kinds.contains(&ArtifactKind::ScoopManifest));
    }

    #[test]
    fn test_print_size_report_filters_and_stores_size() {
        use std::io::Write;

        let dir = std::env::temp_dir().join("anodize_test_size_report");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create real files with known sizes
        let archive_path = dir.join("app.tar.gz");
        let mut f = std::fs::File::create(&archive_path).unwrap();
        f.write_all(&[0u8; 2048]).unwrap();

        let binary_path = dir.join("app");
        let mut f = std::fs::File::create(&binary_path).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();

        let docker_path = dir.join("docker-image");
        let mut f = std::fs::File::create(&docker_path).unwrap();
        f.write_all(&[0u8; 8192]).unwrap();

        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: archive_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: binary_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });
        // DockerImage should be excluded from size reporting
        registry.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: docker_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        print_size_report(&mut registry, &log);

        // Archive and Binary should have size populated
        let archive = &registry.all()[0];
        assert_eq!(archive.kind, ArtifactKind::Archive);
        assert_eq!(archive.size, Some(2048));

        let binary = &registry.all()[1];
        assert_eq!(binary.kind, ArtifactKind::Binary);
        assert_eq!(binary.size, Some(4096));

        // DockerImage should NOT have size populated
        let docker = &registry.all()[2];
        assert_eq!(docker.kind, ArtifactKind::DockerImage);
        assert_eq!(docker.size, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_size_field_defaults_to_none() {
        let registry = ArtifactRegistry::new();
        // Artifact's size is None when freshly constructed
        let mut reg = ArtifactRegistry::new();
        reg.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/nonexistent/binary"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        assert_eq!(reg.all()[0].size, None);
        drop(registry);
    }

    #[test]
    fn test_size_field_not_serialized_when_none() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        let json = registry.to_artifacts_json().unwrap();
        let first = &json.as_array().unwrap()[0];
        // size should not appear in JSON when None
        assert!(first.get("size").is_none());
    }

    #[test]
    fn test_size_field_serialized_when_some() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: Some(12345),
        });
        let json = registry.to_artifacts_json().unwrap();
        let first = &json.as_array().unwrap()[0];
        assert_eq!(first["size"], 12345);
    }
}
