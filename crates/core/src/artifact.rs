use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Binary,
    Archive,
    Checksum,
    DockerImage,
    LinuxPackage,
    Metadata,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub target: Option<String>,
    pub crate_name: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct ArtifactRegistry {
    artifacts: Vec<Artifact>,
}

impl ArtifactRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, artifact: Artifact) {
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

    /// Serialize all artifacts to a JSON value suitable for writing to metadata.json.
    pub fn to_metadata_json(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.artifacts)?)
    }
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
pub fn print_size_report(registry: &ArtifactRegistry) {
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

    eprintln!();
    eprintln!("Artifact Sizes:");
    for (name, size) in &entries {
        eprintln!("  {:<width$}  {}", name, format_size(*size), width = max_name_len);
    }
    eprintln!("  {:<width$}  {}", "Total:", format_size(total), width = max_name_len);
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
            path: PathBuf::from("dist/cfgd"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
        });
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
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
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: PathBuf::from("dist/myapp_checksums.sha256"),
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
}
