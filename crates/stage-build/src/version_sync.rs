use std::path::Path;

use anyhow::{Context as _, Result};

use anodize_core::log::StageLogger;

/// Synchronize the `[package].version` field in a crate's Cargo.toml to the
/// given version string.  Skips writing if the version already matches.
/// In dry-run mode, logs what would happen without modifying the file.
pub fn sync_version(
    crate_path: &str,
    version: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    // Read current version
    let current_version = doc
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if current_version == version {
        log.verbose(&format!(
            "version-sync: {} already at version {}",
            crate_path, version
        ));
        return Ok(());
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) version-sync: would update {} from {} to {}",
            cargo_toml_path.display(),
            current_version,
            version
        ));
        return Ok(());
    }

    // Update the version
    doc["package"]["version"] = toml_edit::value(version);

    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    log.status(&format!(
        "version-sync: updated {} from {} to {}",
        cargo_toml_path.display(),
        current_version,
        version
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::log::Verbosity;

    fn test_logger() -> StageLogger {
        StageLogger::new("build", Verbosity::Normal)
    }

    #[test]
    fn test_sync_version_updates_cargo_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "my-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        sync_version(tmp.path().to_str().unwrap(), "1.2.3", false, &test_logger()).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["package"]["version"].as_str().unwrap(), "1.2.3");
    }

    #[test]
    fn test_sync_version_skips_when_already_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        let original = r#"[package]
name = "my-crate"
version = "1.2.3"
edition = "2024"
"#;
        std::fs::write(&cargo_toml, original).unwrap();

        sync_version(tmp.path().to_str().unwrap(), "1.2.3", false, &test_logger()).unwrap();

        // File should be unchanged
        let content = std::fs::read_to_string(&cargo_toml).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_sync_version_dry_run_does_not_modify() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        let original = r#"[package]
name = "my-crate"
version = "0.1.0"
edition = "2024"
"#;
        std::fs::write(&cargo_toml, original).unwrap();

        sync_version(tmp.path().to_str().unwrap(), "2.0.0", true, &test_logger()).unwrap();

        // File should be unchanged in dry-run mode
        let content = std::fs::read_to_string(&cargo_toml).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_sync_version_missing_cargo_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_version(tmp.path().to_str().unwrap(), "1.0.0", false, &test_logger());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to read"),
            "error should mention read failure, got: {err}"
        );
    }
}
