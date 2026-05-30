//! Derive publisher metadata (description / license / homepage / authors)
//! from a crate's `Cargo.toml [package]` table.
//!
//! Publishers (winget, snapcraft, nfpm, homebrew, nix, chocolatey, scoop,
//! aur, krew, mcp) expose `description` / `license` / `homepage` /
//! `maintainer` fields and fall back to project [`MetadataConfig`] when their
//! own field is unset. A plain Rust project already declares these facts in
//! `Cargo.toml [package]`, so requiring the operator to repeat them in a
//! top-level `metadata:` YAML block is redundant — and omitting them today
//! hard-errors winget ("license is required"), snapcraft ("summary is
//! required"), and nfpm ("maintainer is empty").
//!
//! This module reads each crate's `Cargo.toml [package]` and synthesises a
//! [`MetadataConfig`] holding only the fields it could derive. Resolution
//! precedence (highest first) is enforced by the crate-aware accessors on
//! [`super::Config`]: a per-publisher override wins, then a hand-written
//! top-level `metadata:` field, then this Cargo.toml-derived value.
//!
//! Workspace inheritance (`license.workspace = true`) is resolved against the
//! workspace root's `[workspace.package]` table so a crate that inherits its
//! license/homepage/authors from the workspace still contributes a value.

use std::path::Path;

use toml::Value;

use super::MetadataConfig;

/// Derive a [`MetadataConfig`] from the `Cargo.toml [package]` table at
/// `crate_dir/Cargo.toml`.
///
/// Returns a config holding only the fields that could be read:
/// - `description` ← `package.description`
/// - `license` ← `package.license` (an SPDX string). When the crate uses
///   `license-file` instead, no SPDX id can be synthesised, so `license`
///   is left unset rather than fabricated.
/// - `homepage` ← `package.homepage`, falling back to `package.repository`.
/// - `maintainers` ← `package.authors`.
///
/// A field that is `{ workspace = true }` is resolved against the workspace
/// root's `[workspace.package]` table (searched by walking parent directories
/// for a `Cargo.toml` declaring `[workspace]`).
///
/// Missing / unreadable / unparsable `Cargo.toml` yields an all-`None`
/// [`MetadataConfig`] — derivation is a best-effort enrichment, never a hard
/// failure (the publisher's own "required field" error still fires if nothing
/// supplies the value).
pub fn derive_metadata_from_cargo_toml(crate_dir: &Path) -> MetadataConfig {
    let cargo_toml = crate_dir.join("Cargo.toml");
    let Ok(content) = std::fs::read_to_string(&cargo_toml) else {
        return MetadataConfig::default();
    };
    let Ok(doc) = content.parse::<Value>() else {
        return MetadataConfig::default();
    };

    // Lazily parsed workspace-root `[workspace.package]` table, used only when
    // a `package` field is `{ workspace = true }`.
    let workspace_pkg = WorkspacePackage::resolve(crate_dir);

    let Some(package) = doc.get("package").and_then(Value::as_table) else {
        return MetadataConfig::default();
    };

    let description = string_field(package, &workspace_pkg, "description");

    // `license` is an SPDX expression; `license-file` points at a file whose
    // contents are NOT an SPDX id, so it cannot be synthesised into the
    // `license` slot. Leave empty when only `license-file` is present.
    let license = string_field(package, &workspace_pkg, "license");

    let homepage = string_field(package, &workspace_pkg, "homepage")
        .or_else(|| string_field(package, &workspace_pkg, "repository"));

    let maintainers = string_array_field(package, &workspace_pkg, "authors");

    MetadataConfig {
        description,
        homepage,
        license,
        maintainers,
        mod_timestamp: None,
        full_description: None,
        commit_author: None,
    }
}

type Table = toml::map::Map<String, Value>;

/// The workspace root's `[workspace.package]` table, resolved once so
/// `{ workspace = true }` field inheritance can be honoured.
struct WorkspacePackage {
    table: Option<Table>,
}

impl WorkspacePackage {
    /// Walk up from `crate_dir` looking for a `Cargo.toml` that declares
    /// `[workspace]`. Stops at the filesystem root. The crate's own
    /// `Cargo.toml` is included in the search (single-crate workspaces declare
    /// `[workspace]` alongside `[package]`).
    fn resolve(crate_dir: &Path) -> Self {
        let mut dir = Some(crate_dir);
        while let Some(d) = dir {
            let candidate = d.join("Cargo.toml");
            if let Ok(content) = std::fs::read_to_string(&candidate)
                && let Ok(doc) = content.parse::<Value>()
                && let Some(ws) = doc.get("workspace").and_then(Value::as_table)
            {
                let table = ws.get("package").and_then(Value::as_table).cloned();
                return WorkspacePackage { table };
            }
            dir = d.parent();
        }
        WorkspacePackage { table: None }
    }

    fn string(&self, key: &str) -> Option<String> {
        non_empty_string(self.table.as_ref()?.get(key)?)
    }

    fn string_array(&self, key: &str) -> Option<Vec<String>> {
        string_array_from_value(self.table.as_ref()?.get(key)?)
    }
}

/// Read a `[package]` string field, honouring `{ workspace = true }`
/// inheritance. Empty strings collapse to `None`.
fn string_field(package: &Table, workspace: &WorkspacePackage, key: &str) -> Option<String> {
    match package.get(key) {
        Some(item) if is_workspace_inherited(item) => workspace.string(key),
        Some(item) => non_empty_string(item),
        None => None,
    }
}

/// Read a `[package]` string-array field, honouring `{ workspace = true }`
/// inheritance. Empty arrays collapse to `None`.
fn string_array_field(
    package: &Table,
    workspace: &WorkspacePackage,
    key: &str,
) -> Option<Vec<String>> {
    match package.get(key) {
        Some(item) if is_workspace_inherited(item) => workspace.string_array(key),
        Some(item) => string_array_from_value(item),
        None => None,
    }
}

fn non_empty_string(item: &Value) -> Option<String> {
    item.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

fn string_array_from_value(item: &Value) -> Option<Vec<String>> {
    let arr = item.as_array()?;
    let values: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// `true` when an item is the inline table `{ workspace = true }` used by
/// Cargo for workspace-inherited package fields.
fn is_workspace_inherited(item: &Value) -> bool {
    item.as_table()
        .and_then(|t| t.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn derives_all_fields_from_package_table() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "demo"
description = "a demo crate"
license = "MIT"
homepage = "https://demo.example"
repository = "https://github.com/acme/demo"
authors = ["Ada <ada@example.com>", "Grace"]
"#,
        );
        let m = derive_metadata_from_cargo_toml(dir.path());
        assert_eq!(m.description.as_deref(), Some("a demo crate"));
        assert_eq!(m.license.as_deref(), Some("MIT"));
        assert_eq!(m.homepage.as_deref(), Some("https://demo.example"));
        assert_eq!(
            m.maintainers,
            Some(vec![
                "Ada <ada@example.com>".to_string(),
                "Grace".to_string()
            ])
        );
    }

    #[test]
    fn homepage_falls_back_to_repository() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "demo"
repository = "https://github.com/acme/demo"
"#,
        );
        let m = derive_metadata_from_cargo_toml(dir.path());
        assert_eq!(m.homepage.as_deref(), Some("https://github.com/acme/demo"));
    }

    #[test]
    fn license_file_only_does_not_fabricate_license() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "demo"
license-file = "LICENSE.txt"
"#,
        );
        let m = derive_metadata_from_cargo_toml(dir.path());
        assert!(
            m.license.is_none(),
            "license-file must not synthesise an SPDX id"
        );
    }

    #[test]
    fn resolves_workspace_inherited_fields() {
        let root = tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["crates/demo"]

[workspace.package]
license = "Apache-2.0"
homepage = "https://ws.example"
authors = ["Workspace Author <ws@example.com>"]
"#,
        );
        let crate_dir = root.path().join("crates/demo");
        fs::create_dir_all(&crate_dir).unwrap();
        write(
            &crate_dir,
            "Cargo.toml",
            r#"
[package]
name = "demo"
description = "inherits the rest"
license.workspace = true
homepage.workspace = true
authors.workspace = true
"#,
        );
        let m = derive_metadata_from_cargo_toml(&crate_dir);
        assert_eq!(m.description.as_deref(), Some("inherits the rest"));
        assert_eq!(m.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(m.homepage.as_deref(), Some("https://ws.example"));
        assert_eq!(
            m.maintainers,
            Some(vec!["Workspace Author <ws@example.com>".to_string()])
        );
    }

    #[test]
    fn missing_cargo_toml_yields_empty_metadata() {
        let dir = tempdir().unwrap();
        let m = derive_metadata_from_cargo_toml(dir.path());
        assert!(m.description.is_none());
        assert!(m.license.is_none());
        assert!(m.homepage.is_none());
        assert!(m.maintainers.is_none());
    }

    #[test]
    fn empty_strings_collapse_to_none() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "demo"
description = ""
license = ""
authors = []
"#,
        );
        let m = derive_metadata_from_cargo_toml(dir.path());
        assert!(m.description.is_none());
        assert!(m.license.is_none());
        assert!(m.maintainers.is_none());
    }
}
