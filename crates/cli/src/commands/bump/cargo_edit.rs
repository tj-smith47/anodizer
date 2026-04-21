//! Cargo.toml reading + editing for `anodizer bump`.
//!
//! Uses `toml_edit` to preserve formatting and comments. Responsibilities:
//!   - Load the workspace graph (root Cargo.toml + member manifests).
//!   - Rewrite a member's `[package].version` (or the root
//!     `[workspace.package].version` if the member inherits).
//!   - Rewrite sibling `[dependencies]` / `[dev-dependencies]` /
//!     `[build-dependencies]` entries that reference a bumped member
//!     by its new version (unless `--exact` is set).

use anodizer_core::log::StageLogger;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};

use super::plan::{BumpLevel, PlanRow};

#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub name: String,
    /// Absolute path to the member's `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Absolute path to the crate root directory (parent of manifest).
    pub crate_dir: PathBuf,
    /// Literal version from `[package].version = "X"`, if any.
    pub own_version: Option<String>,
    /// `true` iff the member uses `version.workspace = true`.
    pub inherits_workspace_version: bool,
    /// `[package].publish = false`.
    pub publish_false: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub members: Vec<MemberInfo>,
    pub workspace_package_version: Option<String>,
}

/// Read the workspace root `Cargo.toml` and every member manifest.
pub fn load_workspace(workspace_root: &Path) -> Result<WorkspaceInfo> {
    let root_manifest = workspace_root.join("Cargo.toml");
    let root_text = std::fs::read_to_string(&root_manifest)
        .with_context(|| format!("failed to read {}", root_manifest.display()))?;
    let root_doc = root_text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", root_manifest.display()))?;

    let workspace_package_version = root_doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let member_globs = root_doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut members = Vec::new();
    for pattern in &member_globs {
        let abs_pattern = workspace_root.join(pattern);
        // Glob-expand the pattern so `crates/*` etc. resolve. Plain paths match
        // themselves.
        for entry_path in expand_member_glob(&abs_pattern)? {
            let manifest = entry_path.join("Cargo.toml");
            if !manifest.is_file() {
                continue;
            }
            if let Some(info) = parse_member_manifest(&manifest)? {
                members.push(info);
            }
        }
    }

    // Cargo treats a root manifest with `[package]` as a single-member workspace
    // that contains the package itself, unless an explicit `[workspace]` excludes
    // it. Include the root package if no explicit member already covers it.
    if root_doc.get("package").is_some() {
        let root_has_own_member = members.iter().any(|m| m.manifest_path == root_manifest);
        if !root_has_own_member && let Some(info) = parse_member_manifest(&root_manifest)? {
            members.push(info);
        }
    }

    // Stable ordering: the workspace order is often relied on by users.
    members.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(WorkspaceInfo {
        members,
        workspace_package_version,
    })
}

fn expand_member_glob(pattern: &Path) -> Result<Vec<PathBuf>> {
    let pattern_str = pattern.to_string_lossy();
    if !pattern_str.contains('*') && !pattern_str.contains('?') && !pattern_str.contains('[') {
        return Ok(vec![pattern.to_path_buf()]);
    }
    let mut out = Vec::new();
    for entry in glob::glob(&pattern_str)
        .with_context(|| format!("invalid glob in workspace.members: {}", pattern_str))?
    {
        match entry {
            Ok(p) if p.is_dir() => out.push(p),
            _ => {}
        }
    }
    Ok(out)
}

fn parse_member_manifest(manifest_path: &Path) -> Result<Option<MemberInfo>> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

    let pkg = match doc.get("package").and_then(|p| p.as_table()) {
        Some(p) => p,
        None => return Ok(None), // virtual manifest or non-package manifest
    };

    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .with_context(|| format!("missing [package].name in {}", manifest_path.display()))?;

    let publish_false = pkg
        .get("publish")
        .and_then(|v| v.as_bool())
        .map(|b| !b)
        .unwrap_or(false);

    // Two shapes for version:
    //   version = "1.2.3"          → own_version
    //   version.workspace = true   → inherits_workspace_version
    let (own_version, inherits_workspace_version) = match pkg.get("version") {
        Some(Item::Value(Value::String(s))) => (Some(s.value().to_string()), false),
        Some(Item::Value(Value::InlineTable(t))) if t.get("workspace").is_some() => (
            None,
            t.get("workspace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        Some(Item::Table(t)) if t.get("workspace").is_some() => (
            None,
            t.get("workspace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        _ => (None, false),
    };

    let crate_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    Ok(Some(MemberInfo {
        name,
        manifest_path: manifest_path.to_path_buf(),
        crate_dir,
        own_version,
        inherits_workspace_version,
        publish_false,
    }))
}

/// Apply the plan: rewrite `[package].version` (or root `[workspace.package].version`)
/// and — unless `exact` — propagate dep specs into sibling manifests.
pub fn apply_plan(
    workspace_root: &Path,
    rows: &[PlanRow],
    exact: bool,
    log: &StageLogger,
) -> Result<()> {
    // Group rows into two buckets: root-rewrite (inheriting workspace version)
    // and member-rewrite (own version).
    let ws = load_workspace(workspace_root)?;
    let member_index: BTreeMap<String, &MemberInfo> =
        ws.members.iter().map(|m| (m.name.clone(), m)).collect();

    // Workspace-inheritance check: if multiple inheriting members are bumped
    // to different targets, that's a contradiction.
    let inheriting_bumps: Vec<&PlanRow> = rows
        .iter()
        .filter(|r| r.level != BumpLevel::Skip && r.inherits_workspace_version)
        .collect();
    if inheriting_bumps.len() > 1 {
        let first_next = &inheriting_bumps[0].next;
        for r in &inheriting_bumps[1..] {
            if &r.next != first_next {
                bail!(
                    "crates {} and {} both inherit [workspace.package].version but were bumped to different targets ({} vs {})",
                    inheriting_bumps[0].crate_name,
                    r.crate_name,
                    first_next,
                    r.next
                );
            }
        }
    }

    // 1. Rewrite the root [workspace.package].version if any inheriting crate is bumped.
    if let Some(first) = inheriting_bumps.first() {
        let root_manifest = workspace_root.join("Cargo.toml");
        rewrite_workspace_package_version(&root_manifest, &first.next)?;
        log.verbose(&format!(
            "rewrote [workspace.package].version → {}",
            first.next
        ));
    }

    // 2. Rewrite each member's own [package].version.
    for row in rows {
        if row.level == BumpLevel::Skip || row.inherits_workspace_version {
            continue;
        }
        rewrite_package_version(&row.manifest, &row.next)?;
        log.verbose(&format!(
            "rewrote {} version → {}",
            row.crate_name, row.next
        ));
    }

    // 3. Propagate dep-spec rewrites into sibling manifests (unless exact).
    if !exact {
        let bumped: BTreeMap<String, String> = rows
            .iter()
            .filter(|r| r.level != BumpLevel::Skip)
            .map(|r| (r.crate_name.clone(), r.next.clone()))
            .collect();
        // Root Cargo.toml may carry [workspace.dependencies] — rewrite those too.
        rewrite_workspace_dependencies(&workspace_root.join("Cargo.toml"), &bumped, log)?;
        for m in member_index.values() {
            rewrite_member_dependencies(&m.manifest_path, &bumped, log)?;
        }
    }

    Ok(())
}

fn rewrite_package_version(manifest_path: &Path, new_version: &str) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let pkg = doc
        .get_mut("package")
        .and_then(|p| p.as_table_mut())
        .with_context(|| format!("missing [package] table in {}", manifest_path.display()))?;
    pkg["version"] = toml_edit::value(new_version);
    std::fs::write(manifest_path, doc.to_string())
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

fn rewrite_workspace_package_version(root_manifest: &Path, new_version: &str) -> Result<()> {
    let text = std::fs::read_to_string(root_manifest)
        .with_context(|| format!("failed to read {}", root_manifest.display()))?;
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", root_manifest.display()))?;
    let ws = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .context("root Cargo.toml has no [workspace] table")?;
    let pkg = ws
        .get_mut("package")
        .and_then(|p| p.as_table_mut())
        .context("root Cargo.toml has no [workspace.package] table")?;
    pkg["version"] = toml_edit::value(new_version);
    std::fs::write(root_manifest, doc.to_string())
        .with_context(|| format!("failed to write {}", root_manifest.display()))?;
    Ok(())
}

fn rewrite_member_dependencies(
    manifest_path: &Path,
    bumped: &BTreeMap<String, String>,
    log: &StageLogger,
) -> Result<()> {
    if bumped.is_empty() {
        return Ok(());
    }
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let mut changed = false;
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(tbl) = doc.get_mut(section).and_then(|i| i.as_table_mut()) {
            for (dep_name, new_ver) in bumped {
                if rewrite_dep_entry(tbl, dep_name, new_ver) {
                    log.verbose(&format!(
                        "{}: {} {} = \"{}\"",
                        manifest_path.display(),
                        section,
                        dep_name,
                        new_ver
                    ));
                    changed = true;
                }
            }
        }
    }
    // Also handle [target.*.dependencies] tables.
    if let Some(target) = doc.get_mut("target").and_then(|i| i.as_table_mut()) {
        for (_, item) in target.iter_mut() {
            if let Some(tt) = item.as_table_mut() {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(tbl) = tt.get_mut(section).and_then(|i| i.as_table_mut()) {
                        for (dep_name, new_ver) in bumped {
                            if rewrite_dep_entry(tbl, dep_name, new_ver) {
                                log.verbose(&format!(
                                    "{}: target.{} {} = \"{}\"",
                                    manifest_path.display(),
                                    section,
                                    dep_name,
                                    new_ver
                                ));
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
    }
    if changed {
        std::fs::write(manifest_path, doc.to_string())
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    }
    Ok(())
}

fn rewrite_workspace_dependencies(
    root_manifest: &Path,
    bumped: &BTreeMap<String, String>,
    log: &StageLogger,
) -> Result<()> {
    if bumped.is_empty() {
        return Ok(());
    }
    let text = std::fs::read_to_string(root_manifest)
        .with_context(|| format!("failed to read {}", root_manifest.display()))?;
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", root_manifest.display()))?;
    let Some(ws_deps) = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(|d| d.as_table_mut())
    else {
        return Ok(());
    };
    let mut changed = false;
    for (dep_name, new_ver) in bumped {
        if rewrite_dep_entry(ws_deps, dep_name, new_ver) {
            log.verbose(&format!(
                "root: [workspace.dependencies] {} = \"{}\"",
                dep_name, new_ver
            ));
            changed = true;
        }
    }
    if changed {
        std::fs::write(root_manifest, doc.to_string())
            .with_context(|| format!("failed to write {}", root_manifest.display()))?;
    }
    Ok(())
}

/// Rewrite `<table>[<dep_name>]` to use `new_ver`. Handles three shapes:
///
///   foo = "0.1.0"
///   foo = { version = "0.1.0", ... }
///   foo = { path = "...", version = "0.1.0" }
///
/// Returns `true` if the entry existed and had a version field that was
/// rewritten. Path-only deps (no version field) are left alone.
fn rewrite_dep_entry(tbl: &mut toml_edit::Table, dep_name: &str, new_ver: &str) -> bool {
    let item = match tbl.get_mut(dep_name) {
        Some(i) => i,
        None => return false,
    };
    match item {
        Item::Value(Value::String(_)) => {
            *item = toml_edit::value(new_ver);
            true
        }
        Item::Value(Value::InlineTable(t)) if t.get("version").is_some() => {
            t.insert("version", Value::from(new_ver));
            true
        }
        Item::Table(t) if t.get("version").is_some() => {
            t["version"] = toml_edit::value(new_ver);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn rewrite_literal_version_preserves_format() {
        let dir = tmpdir();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"demo\"\n# comment\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        rewrite_package_version(&manifest, "0.2.0").unwrap();
        let out = std::fs::read_to_string(&manifest).unwrap();
        assert!(out.contains("version = \"0.2.0\""));
        assert!(out.contains("# comment"));
    }

    #[test]
    fn rewrite_workspace_package_version_roundtrip() {
        let dir = tmpdir();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        rewrite_workspace_package_version(&manifest, "0.2.0").unwrap();
        let out = std::fs::read_to_string(&manifest).unwrap();
        assert!(out.contains("version = \"0.2.0\""));
    }

    #[test]
    fn rewrite_dep_entry_handles_three_shapes() {
        let mut doc: DocumentMut = "[dependencies]\nfoo = \"0.1.0\"\nbar = { version = \"0.1.0\", features = [\"x\"] }\nbaz = { path = \"../baz\" }\n".parse().unwrap();
        let tbl = doc.get_mut("dependencies").unwrap().as_table_mut().unwrap();
        assert!(rewrite_dep_entry(tbl, "foo", "0.2.0"));
        assert!(rewrite_dep_entry(tbl, "bar", "0.2.0"));
        // baz has no version → skipped.
        assert!(!rewrite_dep_entry(tbl, "baz", "0.2.0"));
        // nonexistent → skipped.
        assert!(!rewrite_dep_entry(tbl, "qux", "0.2.0"));
        let out = doc.to_string();
        assert!(out.contains("foo = \"0.2.0\""));
        assert!(out.contains("version = \"0.2.0\""));
        assert!(out.contains("path = \"../baz\"")); // unchanged
    }

    #[test]
    fn parse_member_manifest_detects_inherits_workspace() {
        let dir = tmpdir();
        let crate_dir = dir.path().join("a");
        std::fs::create_dir_all(&crate_dir).unwrap();
        let manifest = crate_dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"a\"\nversion.workspace = true\n",
        )
        .unwrap();
        let info = parse_member_manifest(&manifest).unwrap().unwrap();
        assert_eq!(info.name, "a");
        assert!(info.inherits_workspace_version);
        assert!(info.own_version.is_none());
    }

    #[test]
    fn parse_member_manifest_detects_literal_version() {
        let dir = tmpdir();
        let crate_dir = dir.path().join("a");
        std::fs::create_dir_all(&crate_dir).unwrap();
        let manifest = crate_dir.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"a\"\nversion = \"0.3.0\"\n").unwrap();
        let info = parse_member_manifest(&manifest).unwrap().unwrap();
        assert!(!info.inherits_workspace_version);
        assert_eq!(info.own_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn parse_member_manifest_detects_publish_false() {
        let dir = tmpdir();
        let crate_dir = dir.path().join("a");
        std::fs::create_dir_all(&crate_dir).unwrap();
        let manifest = crate_dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\npublish = false\n",
        )
        .unwrap();
        let info = parse_member_manifest(&manifest).unwrap().unwrap();
        assert!(info.publish_false);
    }

    #[test]
    fn load_workspace_finds_members() {
        let dir = tmpdir();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let a = dir.path().join("crates/a");
        let b = dir.path().join("crates/b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            a.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion.workspace = true\n",
        )
        .unwrap();
        std::fs::write(
            b.join("Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.9.0\"\n",
        )
        .unwrap();
        let ws = load_workspace(dir.path()).unwrap();
        assert_eq!(ws.workspace_package_version.as_deref(), Some("0.1.0"));
        let names: Vec<&str> = ws.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(ws.members[0].inherits_workspace_version);
        assert_eq!(ws.members[1].own_version.as_deref(), Some("0.9.0"));
    }
}
