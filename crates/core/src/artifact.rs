use std::collections::HashMap;
use std::path::PathBuf;

use colored::Colorize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Binary,
    Archive,
    Checksum,
    DockerImage,
    DockerManifest,
    LinuxPackage,
    Metadata,
    Signature,
    Certificate,
    Library,
    Wasm,
    SourceArchive,
    Sbom,
    Snap,
    DiskImage,
    Installer,
    MacOsPackage,
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
            ArtifactKind::Archive => "archive",
            ArtifactKind::Checksum => "checksum",
            ArtifactKind::DockerImage => "docker_image",
            ArtifactKind::DockerManifest => "docker_manifest",
            ArtifactKind::LinuxPackage => "linux_package",
            ArtifactKind::Metadata => "metadata",
            ArtifactKind::Signature => "signature",
            ArtifactKind::Certificate => "certificate",
            ArtifactKind::Library => "library",
            ArtifactKind::Wasm => "wasm",
            ArtifactKind::SourceArchive => "source_archive",
            ArtifactKind::Sbom => "sbom",
            ArtifactKind::Snap => "snap",
            ArtifactKind::DiskImage => "disk_image",
            ArtifactKind::Installer => "installer",
            ArtifactKind::MacOsPackage => "macos_package",
        }
    }

    /// Parse a snake_case string into an ArtifactKind.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "binary" => Some(ArtifactKind::Binary),
            "archive" => Some(ArtifactKind::Archive),
            "checksum" => Some(ArtifactKind::Checksum),
            "docker_image" => Some(ArtifactKind::DockerImage),
            "docker_manifest" => Some(ArtifactKind::DockerManifest),
            "linux_package" => Some(ArtifactKind::LinuxPackage),
            "metadata" => Some(ArtifactKind::Metadata),
            "signature" => Some(ArtifactKind::Signature),
            "certificate" => Some(ArtifactKind::Certificate),
            "library" => Some(ArtifactKind::Library),
            "wasm" => Some(ArtifactKind::Wasm),
            "source_archive" => Some(ArtifactKind::SourceArchive),
            "sbom" => Some(ArtifactKind::Sbom),
            "snap" => Some(ArtifactKind::Snap),
            "disk_image" => Some(ArtifactKind::DiskImage),
            "installer" => Some(ArtifactKind::Installer),
            "macos_package" => Some(ArtifactKind::MacOsPackage),
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

    pub fn all(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// Remove all artifacts whose path matches one of the given paths.
    pub fn remove_by_paths(&mut self, paths: &[std::path::PathBuf]) {
        self.artifacts.retain(|a| !paths.contains(&a.path));
    }

    /// Serialize all artifacts to a JSON value suitable for writing to metadata.json.
    pub fn to_metadata_json(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.artifacts)?)
    }
}

/// Artifact kinds that are uploadable to releases/blob storage.
/// Single source of truth — used by release and blob stages.
pub fn uploadable_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::Checksum,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Snap,
        ArtifactKind::DiskImage,
        ArtifactKind::Installer,
        ArtifactKind::MacOsPackage,
        ArtifactKind::SourceArchive,
        ArtifactKind::Sbom,
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

/// Print a formatted size table for all artifacts in the registry.
/// Only includes artifacts whose files exist on disk.
pub fn print_size_report(registry: &ArtifactRegistry, log: &crate::log::StageLogger) {
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut total: u64 = 0;

    for artifact in registry.all() {
        if let Ok(meta) = std::fs::metadata(&artifact.path) {
            let size = meta.len();
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
        });
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/cfgd.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
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
    fn test_to_metadata_json_empty() {
        let registry = ArtifactRegistry::new();
        let json = registry.to_metadata_json().unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_to_metadata_json_with_artifacts() {
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
        });
        registry.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_checksums.txt"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let json = registry.to_metadata_json().unwrap();
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
        });

        let json = registry.to_metadata_json().unwrap();
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
    fn test_query_by_library_and_wasm_kinds() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Library,
            name: String::new(),
            path: PathBuf::from("target/libmylib.so"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
        });
        registry.add(Artifact {
            kind: ArtifactKind::Wasm,
            name: String::new(),
            path: PathBuf::from("target/mylib.wasm"),
            target: Some("wasm32-unknown-unknown".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
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
}
