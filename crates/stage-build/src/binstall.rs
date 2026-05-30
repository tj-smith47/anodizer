use std::path::Path;

use anyhow::{Context as _, Result};

use anodizer_core::config::BinstallConfig;
use anodizer_core::context::Context;

/// Generate or update `[package.metadata.binstall]` in a crate's Cargo.toml
/// based on the provided BinstallConfig.  The `pkg_url` field is rendered
/// through the template engine so that variables like `{{ .Version }}` and
/// `{{ .Target }}` are expanded.
///
/// The update is performed in place: anodize re-writes only the keys it owns
/// (`pkg-url`, `bin-dir`, `pkg-fmt`, and the `overrides` sub-table). Any other
/// key a user added by hand — cargo-binstall's `disabled-strategies`, the
/// `[package.metadata.binstall.signing]` sub-table, or features anodize does
/// not yet model — is preserved verbatim. An owned key that is now unset in
/// config is cleared, but unknown keys still survive.
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

    // Render the anodize-owned values up front so a template error aborts
    // before any mutation (and before the dry-run short-circuit reports
    // success on a config that would have failed).
    let rendered_pkg_url =
        match config.pkg_url {
            Some(ref pkg_url) => Some(ctx.render_template(pkg_url).with_context(|| {
                format!("failed to render binstall pkg_url template: {}", pkg_url)
            })?),
            None => None,
        };

    let rendered_overrides = render_overrides(config, ctx)?;

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

    normalize_binstall_to_table(metadata).with_context(|| {
        format!(
            "[package.metadata.binstall] is neither a table nor an inline table in {}",
            cargo_toml_path.display()
        )
    })?;

    // Merge in place: mutate the existing table so unknown keys
    // (disabled-strategies, signing, future unknowns) survive.
    let binstall = metadata
        .get_mut("binstall")
        .and_then(|b| b.as_table_mut())
        .with_context(|| {
            format!(
                "[package.metadata.binstall] is not a table in {}",
                cargo_toml_path.display()
            )
        })?;

    set_or_remove_str(binstall, "pkg-url", rendered_pkg_url.as_deref());
    set_or_remove_str(binstall, "bin-dir", config.bin_dir.as_deref());
    set_or_remove_str(binstall, "pkg-fmt", config.pkg_fmt.as_deref());

    match rendered_overrides {
        Some(overrides_table) => {
            binstall.insert("overrides", toml_edit::Item::Table(overrides_table));
        }
        None => {
            binstall.remove("overrides");
        }
    }

    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    log.status(&format!(
        "binstall: updated [package.metadata.binstall] in {}",
        cargo_toml_path.display()
    ));

    Ok(())
}

/// Ensure `metadata.binstall` is a header table so the in-place merge can
/// mutate it. Three shapes are handled:
///
/// - **missing** — insert an empty header table.
/// - **header table** — already correct; left untouched.
/// - **inline table** (`binstall = { pkg-url = "…" }`) — converted to a header
///   table, preserving every key/value (including user-authored ones anodize
///   does not model) so an inline-metadata user isn't hard-blocked.
///
/// Returns an error only when `binstall` is present but is neither a table nor
/// an inline table (e.g. a scalar/array), which is a malformed manifest.
fn normalize_binstall_to_table(metadata: &mut toml_edit::Table) -> Result<()> {
    match metadata.get("binstall") {
        None => {
            metadata.insert("binstall", toml_edit::Item::Table(toml_edit::Table::new()));
            Ok(())
        }
        Some(item) if item.is_table() => Ok(()),
        Some(item) => {
            let inline = item.as_inline_table().with_context(|| {
                "[package.metadata.binstall] is neither a table nor an inline table".to_string()
            })?;
            // Rebuild as a header table, carrying every existing key/value
            // (anodize-owned and unknown alike) so nothing is dropped.
            let mut table = toml_edit::Table::new();
            for (k, v) in inline.iter() {
                table.insert(k, toml_edit::Item::Value(v.clone()));
            }
            metadata.insert("binstall", toml_edit::Item::Table(table));
            Ok(())
        }
    }
}

/// Set `key` to `value` when present, or remove it when `None`. Removing a
/// now-unset anodize-owned key keeps the merge faithful to config while
/// leaving sibling unknown keys intact.
fn set_or_remove_str(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            table.insert(key, toml_edit::value(v));
        }
        None => {
            table.remove(key);
        }
    }
}

/// Render the per-target `overrides` sub-table from config, or `None` when no
/// overrides are configured. Override `pkg_url` templates are rendered through
/// the context so anodize tokens expand while cargo-binstall's own `{ ... }`
/// tokens survive intact.
fn render_overrides(config: &BinstallConfig, ctx: &Context) -> Result<Option<toml_edit::Table>> {
    let Some(ref overrides) = config.overrides else {
        return Ok(None);
    };
    if overrides.is_empty() {
        return Ok(None);
    }

    let mut overrides_table = toml_edit::Table::new();
    // Render as proper [package.metadata.binstall.overrides.<triple>]
    // headers rather than an inline dotted key.
    overrides_table.set_implicit(true);
    // BTreeMap iteration is sorted, so emission order is deterministic.
    for (triple, ovr) in overrides {
        let mut entry = toml_edit::Table::new();
        if let Some(ref pkg_url) = ovr.pkg_url {
            let rendered = ctx.render_template(pkg_url).with_context(|| {
                format!("failed to render binstall overrides.{triple} pkg_url template: {pkg_url}")
            })?;
            entry.insert("pkg-url", toml_edit::value(rendered));
        }
        if let Some(ref pkg_fmt) = ovr.pkg_fmt {
            entry.insert("pkg-fmt", toml_edit::value(pkg_fmt.as_str()));
        }
        if let Some(ref bin_dir) = ovr.bin_dir {
            entry.insert("bin-dir", toml_edit::value(bin_dir.as_str()));
        }
        overrides_table.insert(triple, toml_edit::Item::Table(entry));
    }
    Ok(Some(overrides_table))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use anodizer_core::config::{BinstallConfig, BinstallOverride, Config};
    use anodizer_core::context::{Context, ContextOptions};

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
            overrides: None,
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
            overrides: None,
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
            overrides: None,
        };

        let ctx = make_ctx();
        let result =
            generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_generate_binstall_metadata_emits_per_target_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "cfgd"
version = "1.0.0"
edition = "2024"
"#,
        )
        .unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: Some("{ bin }{ binary-ext }".to_string()),
            },
        );
        overrides.insert(
            "aarch64-apple-darwin".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/cfgd/releases/download/v{{ .Version }}/cfgd-{ version }-darwin-arm64.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: None,
            },
        );

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
            overrides: Some(overrides),
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false)
            .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();

        let overrides_item = &doc["package"]["metadata"]["binstall"]["overrides"];
        assert!(
            overrides_item.as_table().is_some(),
            "binstall.overrides should exist as a table"
        );

        // linux-amd64 (go-arch) entry: Version rendered, asset name intact.
        let linux = &overrides_item["x86_64-unknown-linux-gnu"];
        assert!(
            linux.as_table().is_some(),
            "override sub-table should be a real table"
        );
        let linux_url = linux["pkg-url"].as_str().unwrap();
        assert!(
            linux_url.contains("cfgd-1.0.0-linux-amd64.tar.gz"),
            "linux pkg-url should be Version-rendered with go-arch asset name, got: {linux_url}"
        );
        assert!(
            !linux_url.contains("{{ .Version }}"),
            "anodize token should be rendered, got: {linux_url}"
        );
        assert_eq!(linux["pkg-fmt"].as_str().unwrap(), "tgz");
        assert_eq!(linux["bin-dir"].as_str().unwrap(), "{ bin }{ binary-ext }");

        // darwin-arm64 (go-arch) entry.
        let darwin = &overrides_item["aarch64-apple-darwin"];
        assert!(darwin.as_table().is_some());
        let darwin_url = darwin["pkg-url"].as_str().unwrap();
        // The leading v{{ .Version }} is an anodize token (rendered) while
        // `{ version }` is cargo-binstall's own token and must survive intact.
        assert!(
            darwin_url.contains("/v1.0.0/cfgd-{ version }-darwin-arm64.tar.gz"),
            "darwin pkg-url should render the anodize token but leave cargo-binstall's `{{ version }}` intact, got: {darwin_url}"
        );

        // Triple keys contain `-` and must render as proper headers.
        assert!(
            updated.contains("[package.metadata.binstall.overrides.x86_64-unknown-linux-gnu]"),
            "override should render as a [...] header, got:\n{updated}"
        );
    }

    #[test]
    fn test_generate_binstall_metadata_preserves_user_authored_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // Seed a Cargo.toml whose binstall table already carries keys anodize
        // does NOT model: cargo-binstall's `disabled-strategies` and the
        // `[package.metadata.binstall.signing]` sub-table. The in-place merge
        // must leave both untouched while (re)writing pkg-url / overrides.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"

[package.metadata.binstall]
disabled-strategies = ["quick-install", "compile"]
pkg-url = "https://old.example.com/stale"

[package.metadata.binstall.signing]
algorithm = "minisign"
pubkey = "RWQABCDEF1234567890"
"#,
        )
        .unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-linux.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: None,
            },
        );

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: None,
            pkg_fmt: None,
            overrides: Some(overrides),
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false)
            .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // Unknown keys survive verbatim.
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        let strategy_vals: Vec<&str> = strategies.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strategy_vals,
            vec!["quick-install", "compile"],
            "disabled-strategies must survive the merge verbatim"
        );

        let signing = &binstall["signing"];
        assert!(
            signing.as_table().is_some(),
            "signing sub-table must survive the merge"
        );
        assert_eq!(signing["algorithm"].as_str().unwrap(), "minisign");
        assert_eq!(signing["pubkey"].as_str().unwrap(), "RWQABCDEF1234567890");

        // anodize-owned keys are (re)written: pkg-url rendered to the new value.
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("/v1.0.0/myapp-{ target }.tar.gz"),
            "pkg-url should be rewritten with the rendered Version, got: {pkg_url}"
        );
        assert!(
            !pkg_url.contains("old.example.com"),
            "stale anodize-owned pkg-url should be replaced, got: {pkg_url}"
        );

        // overrides is anodize-owned and freshly written.
        let linux = &binstall["overrides"]["x86_64-unknown-linux-gnu"];
        assert_eq!(linux["pkg-fmt"].as_str().unwrap(), "tgz");
        assert!(
            linux["pkg-url"]
                .as_str()
                .unwrap()
                .contains("/v1.0.0/myapp-linux.tar.gz")
        );
    }

    #[test]
    fn test_generate_binstall_metadata_clears_unset_owned_key_keeps_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // pkg-url present plus an unknown sibling key. Config omits pkg_url, so
        // the owned key must be cleared while the unknown key survives.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"

[package.metadata.binstall]
disabled-strategies = ["compile"]
pkg-url = "https://old.example.com/stale"
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false)
            .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        assert!(
            binstall.get("pkg-url").is_none(),
            "unset owned key should be cleared, got:\n{updated}"
        );
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        assert_eq!(strategies.len(), 1, "unknown key must survive clearing");
        assert_eq!(strategies.get(0).unwrap().as_str().unwrap(), "compile");
    }

    #[test]
    fn test_generate_binstall_metadata_merges_inline_table_preserving_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // A user with INLINE binstall metadata: `as_table_mut()` would return
        // None on this. The merge must convert it to a header table while
        // preserving the unknown `disabled-strategies` key.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
metadata.binstall = { pkg-url = "https://old.example.com/stale", disabled-strategies = ["compile"] }
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let ctx = make_ctx();
        generate_binstall_metadata(tmp.path().to_str().unwrap(), &binstall_cfg, &ctx, false)
            .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // pkg-url rewritten with the rendered Version.
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("/v1.0.0/myapp-{ target }.tar.gz")
                && !pkg_url.contains("old.example.com"),
            "inline pkg-url should be rewritten, got: {pkg_url}"
        );
        // Unknown key carried over from the inline table.
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies.get(0).unwrap().as_str().unwrap(), "compile");
    }
}
