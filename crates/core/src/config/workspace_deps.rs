//! Derive a crate's intra-workspace `depends_on` from its `Cargo.toml`, and
//! discover the full on-disk Cargo workspace membership.
//!
//! [`extract_workspace_deps`] is the one implementation shared by three call
//! sites: `anodizer init`'s scaffold-time `depends_on` generation (the CLI
//! crate's `commands::init`), [`super::Config::populate_derived_depends_on`]'s
//! config-load derivation for any crate entry that OMITS `depends_on`, and
//! `anodizer check config`'s workspace-membership guard — the v0.19.0 class
//! of failure where a crate is a real Cargo dependency of a published crate
//! but absent from `crates:`, so cargo fails the dependent crate's publish
//! upload (`no matching package named ... found`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use toml::Value;

/// Extract the subset of a parsed `[dependencies]` table that are
/// intra-workspace deps: entries carrying `path = "..."` or
/// `workspace = true`, whose package name is also present in
/// `member_names`. `workspace = true` alone does not imply a local crate —
/// it just inherits from `[workspace.dependencies]`, which can pin an
/// external crate — so the name check against known members disambiguates.
/// Sorted for deterministic output.
pub fn extract_workspace_deps(
    dependencies: Option<&Value>,
    member_names: &HashSet<String>,
) -> Vec<String> {
    let mut deps = vec![];
    if let Some(Value::Table(table)) = dependencies {
        for (dep_name, val) in table {
            let is_member = member_names.contains(dep_name) && {
                match val {
                    Value::Table(t) => {
                        t.contains_key("path")
                            || t.get("workspace")
                                .is_some_and(|v| v.as_bool() == Some(true))
                    }
                    _ => false,
                }
            };
            if is_member {
                deps.push(dep_name.clone());
            }
        }
    }
    deps.sort();
    deps
}

/// Expand a glob pattern relative to `root` (only handles trailing `*`
/// patterns, matching Cargo's own `members = ["crates/*"]` shorthand).
/// Returns paths relative to `root`.
fn expand_glob(root: &Path, pattern: &str) -> Vec<String> {
    if pattern.contains('*') {
        let prefix = pattern.trim_end_matches('*').trim_end_matches('/');
        let dir = root.join(prefix);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            return entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| {
                    e.path()
                        .strip_prefix(root)
                        .ok()
                        .map(|p| p.to_string_lossy().to_string())
                })
                .collect();
        }
        vec![]
    } else {
        vec![pattern.to_string()]
    }
}

fn read_toml(path: PathBuf) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str::<Value>(&content).ok()
}

fn package_name(doc: &Value) -> Option<String> {
    doc.get("package")
        .and_then(Value::as_table)
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Discover every on-disk Cargo workspace member's package name by reading
/// `<base_dir>/Cargo.toml`'s `[workspace] members` (glob-expanding trailing
/// `*` patterns) and each member's own `[package].name`. A single-crate
/// project (no `[workspace]` table) yields just the root package's own
/// name — there is no other on-disk crate a dependency could silently drop
/// out of `crates:`. Missing / unparsable `Cargo.toml` yields an empty set
/// (best-effort, matches [`derive_metadata_from_cargo_toml`]'s resilience
/// contract).
///
/// [`derive_metadata_from_cargo_toml`]: super::cargo_metadata::derive_metadata_from_cargo_toml
pub fn discover_cargo_workspace_member_names(base_dir: &Path) -> HashSet<String> {
    let Some(root) = read_toml(base_dir.join("Cargo.toml")) else {
        return HashSet::new();
    };

    let Some(ws_table) = root.get("workspace").and_then(Value::as_table) else {
        return package_name(&root).into_iter().collect();
    };
    let Some(members) = ws_table.get("members").and_then(Value::as_array) else {
        return package_name(&root).into_iter().collect();
    };

    let excluded: HashSet<String> = ws_table
        .get("exclude")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .flat_map(|pattern| expand_glob(base_dir, pattern))
                .collect()
        })
        .unwrap_or_default();

    let mut names = HashSet::new();
    for pattern in members.iter().filter_map(Value::as_str) {
        for member_path in expand_glob(base_dir, pattern) {
            if excluded.contains(&member_path) {
                continue;
            }
            let cargo_path = base_dir.join(&member_path).join("Cargo.toml");
            if let Some(cargo) = read_toml(cargo_path)
                && let Some(name) = package_name(&cargo)
            {
                names.insert(name);
            }
        }
    }
    names
}

/// Read `<crate_dir>/Cargo.toml`'s `[dependencies]`, `[build-dependencies]`,
/// and every `[target.'cfg(...)'.dependencies]` table, and derive the union
/// of their intra-workspace deps against `member_names` via
/// [`extract_workspace_deps`]. Build- and target-gated deps are resolved by
/// `cargo publish`'s verification build exactly like ordinary deps, so a
/// workspace member reachable only through one of them is an equally hard
/// publish-order requirement. `[dev-dependencies]` is deliberately excluded:
/// it is never resolved for a publish upload. Missing / unparsable
/// `Cargo.toml` yields an empty list (best-effort).
pub fn derive_depends_on_from_cargo_toml(
    crate_dir: &Path,
    member_names: &HashSet<String>,
) -> Vec<String> {
    let Some(doc) = read_toml(crate_dir.join("Cargo.toml")) else {
        return vec![];
    };

    let mut deps: HashSet<String> = extract_workspace_deps(doc.get("dependencies"), member_names)
        .into_iter()
        .collect();
    deps.extend(extract_workspace_deps(
        doc.get("build-dependencies"),
        member_names,
    ));
    if let Some(Value::Table(targets)) = doc.get("target") {
        for target_table in targets.values() {
            deps.extend(extract_workspace_deps(
                target_table.get("dependencies"),
                member_names,
            ));
        }
    }

    let mut deps: Vec<String> = deps.into_iter().collect();
    deps.sort();
    deps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn extract_workspace_deps_matches_path_and_workspace_true_members() {
        let deps: Value = toml::from_str(
            r#"
            local-path = { path = "../local-path" }
            local-inherited.workspace = true
            external = "1.0"
            not-a-member.workspace = true
            "#,
        )
        .unwrap();
        let members: HashSet<String> = ["local-path", "local-inherited"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            extract_workspace_deps(Some(&deps), &members),
            vec!["local-inherited".to_string(), "local-path".to_string()]
        );
    }

    #[test]
    fn extract_workspace_deps_ignores_workspace_true_dep_not_a_known_member() {
        // `workspace = true` alone inherits a pinned version from
        // `[workspace.dependencies]`; it does not imply the dependency is a
        // local crate unless its name is also a known workspace member.
        let deps: Value = toml::from_str(r#"serde.workspace = true"#).unwrap();
        let members: HashSet<String> = HashSet::new();
        assert!(extract_workspace_deps(Some(&deps), &members).is_empty());
    }

    #[test]
    fn discover_cargo_workspace_member_names_reads_declared_members() {
        let root = tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            r#"
            [workspace]
            members = ["crates/a", "crates/b"]
            "#,
        );
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n",
        );
        write(
            root.path(),
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\n",
        );

        let names = discover_cargo_workspace_member_names(root.path());
        assert_eq!(names, ["a", "b"].into_iter().map(String::from).collect());
    }

    #[test]
    fn discover_cargo_workspace_member_names_honors_workspace_exclude() {
        // Cargo's own [workspace] exclude removes a glob-matched directory
        // from the workspace even though it lives under the glob's prefix —
        // member discovery must mirror that or falsely treat an excluded
        // (non-member, non-published) directory as a real dependency.
        let root = tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/broken\"]\n",
        );
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n",
        );
        write(
            root.path(),
            "crates/broken/Cargo.toml",
            "[package]\nname = \"broken\"\n",
        );

        let names = discover_cargo_workspace_member_names(root.path());
        assert_eq!(names, HashSet::from(["a".to_string()]));
    }

    #[test]
    fn discover_cargo_workspace_member_names_expands_glob_members() {
        let root = tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n",
        );
        write(
            root.path(),
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\n",
        );

        let names = discover_cargo_workspace_member_names(root.path());
        assert_eq!(names, ["a", "b"].into_iter().map(String::from).collect());
    }

    #[test]
    fn discover_cargo_workspace_member_names_single_crate_yields_own_name_only() {
        let root = tempdir().unwrap();
        write(root.path(), "Cargo.toml", "[package]\nname = \"solo\"\n");
        let names = discover_cargo_workspace_member_names(root.path());
        assert_eq!(names, HashSet::from(["solo".to_string()]));
    }

    #[test]
    fn discover_cargo_workspace_member_names_missing_cargo_toml_is_empty() {
        let root = tempdir().unwrap();
        assert!(discover_cargo_workspace_member_names(root.path()).is_empty());
    }

    #[test]
    fn derive_depends_on_from_cargo_toml_reads_intra_workspace_deps() {
        let root = tempdir().unwrap();
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n[dependencies]\nb.workspace = true\nserde = \"1\"\n",
        );
        let members: HashSet<String> = ["a", "b"].into_iter().map(String::from).collect();
        let deps = derive_depends_on_from_cargo_toml(&root.path().join("crates/a"), &members);
        assert_eq!(deps, vec!["b".to_string()]);
    }

    #[test]
    fn derive_depends_on_from_cargo_toml_reads_build_dependencies() {
        // A proc-macro/build-helper workspace member reachable ONLY via
        // [build-dependencies] is an equally hard publish-order requirement
        // (cargo resolves build-deps at publish time too) — the v0.19.0
        // failure class reproduces here just as it does for [dependencies].
        let root = tempdir().unwrap();
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n[build-dependencies]\nbuild-helper.workspace = true\n",
        );
        let members: HashSet<String> = ["a", "build-helper"]
            .into_iter()
            .map(String::from)
            .collect();
        let deps = derive_depends_on_from_cargo_toml(&root.path().join("crates/a"), &members);
        assert_eq!(deps, vec!["build-helper".to_string()]);
    }

    #[test]
    fn derive_depends_on_from_cargo_toml_reads_target_cfg_dependencies() {
        // [target.'cfg(...)'.dependencies] deps are equally hard requirements
        // — cargo resolves them for publish verification regardless of the
        // host platform doing the publishing.
        let root = tempdir().unwrap();
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n\
             [target.'cfg(windows)'.dependencies]\nwin-helper.workspace = true\n",
        );
        let members: HashSet<String> = ["a", "win-helper"].into_iter().map(String::from).collect();
        let deps = derive_depends_on_from_cargo_toml(&root.path().join("crates/a"), &members);
        assert_eq!(deps, vec!["win-helper".to_string()]);
    }

    #[test]
    fn derive_depends_on_from_cargo_toml_excludes_dev_dependencies() {
        // dev-dependencies are never resolved for a `cargo publish` upload —
        // must stay excluded even after extending to build/target tables.
        let root = tempdir().unwrap();
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n[dev-dependencies]\ntest-helper.workspace = true\n",
        );
        let members: HashSet<String> = ["a", "test-helper"].into_iter().map(String::from).collect();
        let deps = derive_depends_on_from_cargo_toml(&root.path().join("crates/a"), &members);
        assert!(deps.is_empty());
    }

    #[test]
    fn derive_depends_on_from_cargo_toml_merges_and_dedupes_all_tables() {
        let root = tempdir().unwrap();
        write(
            root.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n\
             [dependencies]\nshared.workspace = true\nb.workspace = true\n\
             [build-dependencies]\nshared.workspace = true\n\
             [target.'cfg(unix)'.dependencies]\nc.workspace = true\n",
        );
        let members: HashSet<String> = ["a", "b", "c", "shared"]
            .into_iter()
            .map(String::from)
            .collect();
        let deps = derive_depends_on_from_cargo_toml(&root.path().join("crates/a"), &members);
        assert_eq!(
            deps,
            vec!["b".to_string(), "c".to_string(), "shared".to_string()]
        );
    }
}
