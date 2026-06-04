use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use anodizer_core::log::StageLogger;

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

/// Read the current `[package].version` from a crate's Cargo.toml, falling back
/// to `"0.0.0"` when the manifest has no literal `[package].version` (e.g. a
/// virtual or workspace-inheriting manifest).
pub fn read_cargo_version(crate_path: &str) -> Result<String> {
    Ok(read_cargo_version_opt(crate_path)?.unwrap_or_else(|| "0.0.0".to_string()))
}

/// Read the literal `[package].version` from a crate's Cargo.toml, returning
/// `None` when the manifest is present but carries no literal version string
/// (a virtual manifest, or one using `version.workspace = true`).
///
/// Unlike [`read_cargo_version`] this does NOT substitute a `"0.0.0"` sentinel:
/// callers that must distinguish "no version declared here" from a real version
/// (e.g. a coherence check comparing sibling versions) need the absence
/// preserved so a versionless member is skipped rather than compared as
/// `0.0.0`.
pub fn read_cargo_version_opt(crate_path: &str) -> Result<Option<String>> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;
    Ok(doc
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// Recursively find all Cargo.toml files, excluding root and target dirs.
///
/// A nested directory containing a `Cargo.toml` with a `[workspace]` table is
/// treated as an INDEPENDENT Cargo workspace root and its subtree is NOT
/// descended into: path-dep version pins only resolve within a single Cargo
/// workspace, so a bump in this workspace must never rewrite a pin owned by a
/// sibling workspace on its own release cadence. The starting `root` itself is
/// always descended (its own `[workspace]` is the boundary we're scoping to).
fn find_cargo_tomls(dir: &Path, root_toml: &Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default();
            if name == "target" || name == ".git" || name == "dist" {
                continue;
            }
            // A subdirectory that is itself a Cargo workspace root delimits a
            // separate release group; do not cross the boundary.
            if dir_is_workspace_root(&path) {
                continue;
            }
            find_cargo_tomls(&path, root_toml, out);
        } else if path.file_name().map(|n| n == "Cargo.toml").unwrap_or(false) && path != root_toml
        {
            out.push(path);
        }
    }
}

/// Returns true when `dir/Cargo.toml` declares a `[workspace]` table, marking
/// `dir` as an independent Cargo workspace root.
fn dir_is_workspace_root(dir: &Path) -> bool {
    let manifest = dir.join("Cargo.toml");
    std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .map(|doc| doc.get("workspace").is_some())
        .unwrap_or(false)
}

/// Resolve the Cargo workspace root that owns `crate_dir`: the nearest ancestor
/// (including `crate_dir` itself) whose `Cargo.toml` declares a `[workspace]`
/// table, bounded above by `repo_root`. Falls back to `repo_root` when no such
/// ancestor exists (e.g. a flat single-crate repo whose root manifest is a
/// plain `[package]`).
///
/// This is the scoping unit for intra-workspace dependency-pin propagation:
/// path-dep `version` pins are Cargo-workspace-local, so a crate bump may only
/// rewrite pins inside its own workspace, never a sibling group on a different
/// cadence.
pub fn cargo_workspace_root_for(repo_root: &Path, crate_dir: &Path) -> PathBuf {
    let repo_root_canon =
        std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let mut cur = std::fs::canonicalize(crate_dir).unwrap_or_else(|_| crate_dir.to_path_buf());

    loop {
        if dir_is_workspace_root(&cur) {
            return cur;
        }
        if cur == repo_root_canon {
            break;
        }
        match cur.parent() {
            Some(parent) if parent.starts_with(&repo_root_canon) || parent == repo_root_canon => {
                cur = parent.to_path_buf();
            }
            _ => break,
        }
    }
    repo_root_canon
}

/// Update intra-workspace dependency version specs for a given crate name.
///
/// The scan is confined to the Cargo workspace that owns `crate_dir` — the
/// nearest ancestor (bounded by `repo_root`) whose `Cargo.toml` declares a
/// `[workspace]` table. Within that scope, every `[dependencies]`,
/// `[dev-dependencies]`, and `[build-dependencies]` entry that references
/// `crate_name` with a `path` key (a workspace-local dep) has its `version`
/// field updated to match the new version.
///
/// Scoping to the owning Cargo workspace is what keeps a bump in one release
/// group from rewriting a path-dep pin in an independent group on a separate
/// cadence: path-dep `version` pins only resolve within a single Cargo
/// workspace, so a sibling workspace's pins are never in this crate's scope.
///
/// Returns a list of modified file paths (for staging).
pub fn sync_workspace_deps(
    repo_root: &str,
    crate_dir: &str,
    crate_name: &str,
    version: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let mut modified = Vec::new();
    let repo_root = Path::new(repo_root);
    let scope_root = cargo_workspace_root_for(repo_root, Path::new(crate_dir));

    // Find all Cargo.toml files under the owning workspace root, stopping at
    // any nested independent workspace boundary.
    let mut cargo_tomls = Vec::new();
    find_cargo_tomls(
        &scope_root,
        &scope_root.join("Cargo.toml"),
        &mut cargo_tomls,
    );

    for path in &cargo_tomls {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut doc = match content.parse::<toml_edit::DocumentMut>() {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut changed = false;
        for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
            let needs_update = doc
                .get(section)
                .and_then(|d| d.as_table())
                .and_then(|deps| deps.get(crate_name))
                .map(|dep| {
                    let has_path = dep.get("path").is_some();
                    let cur_ver = dep.get("version").and_then(|v| v.as_str());
                    has_path && cur_ver.is_some_and(|v| v != version)
                })
                .unwrap_or(false);

            if needs_update {
                let dep = &mut doc[section][crate_name];
                if let Some(tbl) = dep.as_inline_table_mut() {
                    tbl.insert("version", version.into());
                } else if let Some(tbl) = dep.as_table_mut() {
                    tbl.insert("version", toml_edit::Item::Value(version.into()));
                }
                changed = true;
            }
        }

        if changed {
            let path_str = path.to_string_lossy().to_string();
            if dry_run {
                log.status(&format!(
                    "(dry-run) version-sync: would update {} dep in {}",
                    crate_name,
                    path.display()
                ));
            } else {
                std::fs::write(path, doc.to_string())
                    .with_context(|| format!("failed to write {}", path.display()))?;
                log.status(&format!(
                    "version-sync: updated {} dep in {}",
                    crate_name,
                    path.display()
                ));
            }
            modified.push(path_str);
        }
    }

    Ok(modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::Verbosity;

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
    fn read_cargo_version_opt_distinguishes_present_from_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().to_str().unwrap();

        // Literal version present → Some(version).
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.3.1\"\n",
        )
        .unwrap();
        assert_eq!(read_cargo_version_opt(p).unwrap().as_deref(), Some("0.3.1"));

        // Workspace-inheriting / no literal version → None (NOT the 0.0.0
        // sentinel `read_cargo_version` substitutes).
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion.workspace = true\n",
        )
        .unwrap();
        assert_eq!(read_cargo_version_opt(p).unwrap(), None);
        assert_eq!(read_cargo_version(p).unwrap(), "0.0.0");
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

    fn write_toml(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn version_of_dep(manifest: &Path, section: &str, dep: &str) -> Option<String> {
        let doc = std::fs::read_to_string(manifest)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc.get(section)
            .and_then(|s| s.get(dep))
            .and_then(|d| d.get("version"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Two independent Cargo workspaces under one repo root. Group B
    /// path-depends on group A's crate. Bumping group A's crate must rewrite
    /// the within-group-A dependent's pin but MUST NOT touch group B's pin —
    /// B is on a separate release cadence.
    #[test]
    fn test_sync_workspace_deps_does_not_cross_workspace_boundary() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();

        // Group A: its own Cargo workspace at group-a/.
        write_toml(
            &root.join("group-a/Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"app\"]\n",
        );
        write_toml(
            &root.join("group-a/core/Cargo.toml"),
            "[package]\nname = \"a-core\"\nversion = \"0.1.0\"\n",
        );
        // Within-group-A dependent: pins a-core via path+version.
        write_toml(
            &root.join("group-a/app/Cargo.toml"),
            "[package]\nname = \"a-app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\na-core = { path = \"../core\", version = \"0.1.0\" }\n",
        );

        // Group B: a SEPARATE Cargo workspace at group-b/, on its own cadence,
        // that also path-depends on group A's a-core.
        write_toml(
            &root.join("group-b/Cargo.toml"),
            "[workspace]\nmembers = [\"b\"]\n",
        );
        write_toml(
            &root.join("group-b/b/Cargo.toml"),
            "[package]\nname = \"b-crate\"\nversion = \"9.9.9\"\n\n\
             [dependencies]\na-core = { path = \"../../group-a/core\", version = \"0.1.0\" }\n",
        );

        let crate_dir = root.join("group-a/core");
        let modified = sync_workspace_deps(
            root.to_str().unwrap(),
            crate_dir.to_str().unwrap(),
            "a-core",
            "0.2.0",
            false,
            &test_logger(),
        )
        .unwrap();

        // Within-group-A dependent IS updated.
        assert_eq!(
            version_of_dep(
                &root.join("group-a/app/Cargo.toml"),
                "dependencies",
                "a-core"
            )
            .as_deref(),
            Some("0.2.0"),
            "within-group-A dependent's pin must be propagated"
        );

        // Group B's pin is UNTOUCHED — bumping group A never rewrites group B.
        assert_eq!(
            version_of_dep(&root.join("group-b/b/Cargo.toml"), "dependencies", "a-core").as_deref(),
            Some("0.1.0"),
            "independent group B's pin must NOT be rewritten by a group-A bump"
        );

        // The modified list reflects only the in-scope manifest.
        assert!(
            modified
                .iter()
                .any(|m| m.contains("group-a") && m.ends_with("app/Cargo.toml")),
            "group-a/app should be reported modified, got {modified:?}"
        );
        assert!(
            !modified.iter().any(|m| m.contains("group-b")),
            "no group-b manifest should be reported modified, got {modified:?}"
        );
    }

    /// Regression guard: a single flat Cargo workspace still propagates pins to
    /// every sibling member (within-group propagation must be preserved).
    #[test]
    fn test_sync_workspace_deps_single_workspace_propagates() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();

        write_toml(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"cli\"]\n",
        );
        write_toml(
            &root.join("core/Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"1.0.0\"\n",
        );
        write_toml(
            &root.join("cli/Cargo.toml"),
            "[package]\nname = \"cli\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\ncore = { path = \"../core\", version = \"1.0.0\" }\n",
        );

        let crate_dir = root.join("core");
        sync_workspace_deps(
            root.to_str().unwrap(),
            crate_dir.to_str().unwrap(),
            "core",
            "1.1.0",
            false,
            &test_logger(),
        )
        .unwrap();

        assert_eq!(
            version_of_dep(&root.join("cli/Cargo.toml"), "dependencies", "core").as_deref(),
            Some("1.1.0"),
            "sibling member's pin must be propagated within a single workspace"
        );
    }

    /// Mutation-effective guard for the `find_cargo_tomls` boundary-stop:
    /// a nested INDEPENDENT workspace under the bumped crate's own workspace
    /// root. Here `cargo_workspace_root_for` resolves to the root workspace, so
    /// the scan walks the whole root tree and DOES encounter `nested/` — only
    /// the boundary-stop (`if dir_is_workspace_root(&path) { continue; }`)
    /// prevents the nested workspace's pin from being rewritten. Without that
    /// line this test fails (the nested pin gets clobbered).
    #[test]
    fn test_sync_workspace_deps_prunes_nested_workspace() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();

        // Root workspace: core + app, plus a nested member glob.
        write_toml(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"app\", \"nested/n\"]\n",
        );
        write_toml(
            &root.join("core/Cargo.toml"),
            "[package]\nname = \"r-core\"\nversion = \"0.1.0\"\n",
        );
        // Same-workspace dependent: pins r-core via path+version.
        write_toml(
            &root.join("app/Cargo.toml"),
            "[package]\nname = \"r-app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nr-core = { path = \"../core\", version = \"0.1.0\" }\n",
        );
        // Nested INDEPENDENT workspace under the root, on its own cadence,
        // that also path-depends on the root's r-core.
        write_toml(
            &root.join("nested/Cargo.toml"),
            "[workspace]\nmembers = [\"n\"]\n",
        );
        write_toml(
            &root.join("nested/n/Cargo.toml"),
            "[package]\nname = \"nested-n\"\nversion = \"5.0.0\"\n\n\
             [dependencies]\nr-core = { path = \"../../core\", version = \"0.1.0\" }\n",
        );

        let crate_dir = root.join("core");
        let modified = sync_workspace_deps(
            root.to_str().unwrap(),
            crate_dir.to_str().unwrap(),
            "r-core",
            "0.2.0",
            false,
            &test_logger(),
        )
        .unwrap();

        // Same-workspace dependent IS updated.
        assert_eq!(
            version_of_dep(&root.join("app/Cargo.toml"), "dependencies", "r-core").as_deref(),
            Some("0.2.0"),
            "same-workspace dependent's pin must be propagated"
        );

        // Nested independent workspace's pin is PRUNED by the boundary-stop.
        assert_eq!(
            version_of_dep(&root.join("nested/n/Cargo.toml"), "dependencies", "r-core").as_deref(),
            Some("0.1.0"),
            "nested independent workspace's pin must NOT be rewritten (boundary-stop)"
        );

        assert!(
            modified
                .iter()
                .any(|m| m.ends_with("app/Cargo.toml") && !m.contains("nested")),
            "root/app should be reported modified, got {modified:?}"
        );
        assert!(
            !modified.iter().any(|m| m.contains("nested")),
            "no nested manifest should be reported modified, got {modified:?}"
        );
    }
}
