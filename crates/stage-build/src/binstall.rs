use std::path::Path;

use anyhow::{Context as _, Result};

use anodize_core::config::BinstallConfig;
use anodize_core::context::Context;

/// Generate or update `[package.metadata.binstall]` in a crate's Cargo.toml
/// based on the provided BinstallConfig.  The `pkg_url` field is rendered
/// through the template engine so that variables like `{{ .Version }}` and
/// `{{ .Target }}` are expanded.
pub fn generate_binstall_metadata(
    crate_path: &str,
    config: &BinstallConfig,
    ctx: &Context,
    dry_run: bool,
) -> Result<()> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    // Build the binstall metadata table
    let mut binstall_table = toml_edit::InlineTable::new();

    if let Some(ref pkg_url) = config.pkg_url {
        let rendered = ctx
            .render_template(pkg_url)
            .with_context(|| format!("failed to render binstall pkg_url template: {}", pkg_url))?;
        binstall_table.insert("pkg-url", toml_edit::Value::from(rendered));
    }

    if let Some(ref bin_dir) = config.bin_dir {
        binstall_table.insert("bin-dir", toml_edit::Value::from(bin_dir.as_str()));
    }

    if let Some(ref pkg_fmt) = config.pkg_fmt {
        binstall_table.insert("pkg-fmt", toml_edit::Value::from(pkg_fmt.as_str()));
    }

    let log = ctx.logger("build");
    if dry_run {
        log.status(&format!(
            "(dry-run) binstall: would update [package.metadata.binstall] in {}",
            cargo_toml_path.display()
        ));
        return Ok(());
    }

    // Ensure [package.metadata] exists
    let package = doc
        .get_mut("package")
        .and_then(|p| p.as_table_mut())
        .with_context(|| format!("no [package] table in {}", cargo_toml_path.display()))?;

    if !package.contains_key("metadata") {
        package.insert("metadata", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let metadata = package
        .get_mut("metadata")
        .and_then(|m| m.as_table_mut())
        .with_context(|| {
            format!(
                "[package].metadata is not a table in {}",
                cargo_toml_path.display()
            )
        })?;

    // Insert the binstall table (replace if exists)
    let mut table = toml_edit::Table::new();
    for (k, v) in binstall_table.iter() {
        table.insert(k, toml_edit::Item::Value(v.clone()));
    }
    metadata.insert("binstall", toml_edit::Item::Table(table));

    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    log.status(&format!(
        "binstall: updated [package.metadata.binstall] in {}",
        cargo_toml_path.display()
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{BinstallConfig, Config};
    use anodize_core::context::{Context, ContextOptions};

    fn make_ctx() -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx
    }

    #[test]
    fn test_generate_binstall_metadata_inserts_section() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{{ .Version }}-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: Some("{ bin }{ binary-ext }".to_string()),
            pkg_fmt: Some("tgz".to_string()),
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false)
            .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();

        let binstall = &doc["package"]["metadata"]["binstall"];
        assert!(
            binstall.as_table().is_some(),
            "binstall section should exist as a table"
        );
        assert_eq!(binstall["pkg-fmt"].as_str().unwrap(), "tgz");
        assert_eq!(
            binstall["bin-dir"].as_str().unwrap(),
            "{ bin }{ binary-ext }"
        );
        // The pkg-url should have the template variable rendered
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("1.0.0"),
            "pkg-url should have Version rendered, got: {pkg_url}"
        );
    }

    #[test]
    fn test_generate_binstall_metadata_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        let original = r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
"#;
        std::fs::write(&cargo_toml, original).unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some("https://example.com".to_string()),
            bin_dir: None,
            pkg_fmt: None,
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, true)
            .unwrap();

        // File should be unchanged in dry-run mode
        let content = std::fs::read_to_string(&cargo_toml).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_generate_binstall_metadata_missing_cargo_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
        };

        let ctx = make_ctx();
        let result =
            generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false);
        assert!(result.is_err());
    }
}
