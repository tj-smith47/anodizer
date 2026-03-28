use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------------------
// Data types for reading Cargo.toml files
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, Default)]
struct CargoToml {
    workspace: Option<CargoWorkspace>,
    package: Option<CargoPackage>,
    bin: Option<Vec<CargoBin>>,
    dependencies: Option<toml::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct CargoWorkspace {
    members: Option<Vec<String>>,
    package: Option<CargoPackage>,
}

#[derive(Debug, serde::Deserialize, Clone)]
struct CargoPackage {
    name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct CargoBin {}

// ---------------------------------------------------------------------------
// CrateInfo — intermediate representation
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CrateInfo {
    name: String,
    path: String,
    is_binary: bool,
    depends_on: Vec<String>, // workspace member deps
}

// ---------------------------------------------------------------------------
// Main run() implementation
// ---------------------------------------------------------------------------

pub fn run() -> Result<()> {
    let yaml = generate_config(".")?;
    print!("{}", yaml);
    Ok(())
}

/// Generate anodize.yaml content from a directory root.
/// Exposed for testing.
pub fn generate_config(root: &str) -> Result<String> {
    let root_path = Path::new(root);
    let cargo_path = root_path.join("Cargo.toml");
    let cargo_content = std::fs::read_to_string(&cargo_path)
        .with_context(|| format!("cannot read {}", cargo_path.display()))?;
    let root_cargo: CargoToml = toml::from_str(&cargo_content)
        .with_context(|| format!("cannot parse {}", cargo_path.display()))?;

    let crates = if let Some(ws) = &root_cargo.workspace {
        // Workspace project
        discover_workspace_crates(root, ws)?
    } else {
        // Single-crate project
        let name = root_cargo
            .package
            .as_ref()
            .and_then(|p| p.name.as_deref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                root_path
                    .canonicalize()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "project".to_string())
            });
        let is_binary = root_cargo
            .bin
            .as_ref()
            .map(|b| !b.is_empty())
            .unwrap_or(false)
            || root_path.join("src/main.rs").exists();
        vec![CrateInfo {
            name: name.clone(),
            path: ".".to_string(),
            is_binary,
            depends_on: vec![],
        }]
    };

    let project_name = root_cargo
        .workspace
        .as_ref()
        .and_then(|ws| ws.package.as_ref())
        .and_then(|p| p.name.as_deref())
        .map(|s| s.to_string())
        .or_else(|| {
            root_cargo
                .package
                .as_ref()
                .and_then(|p| p.name.as_deref())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| {
            root_path
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "project".to_string())
        });

    // Build topological order for depends_on resolution
    let sorted = topological_sort(&crates);
    render_yaml(&project_name, &sorted)
}

fn discover_workspace_crates(root: &str, ws: &CargoWorkspace) -> Result<Vec<CrateInfo>> {
    let root_path = Path::new(root);
    let members = ws.members.as_deref().unwrap_or(&[]);

    // Collect all member names first for dep-graph resolution
    let mut member_names: HashSet<String> = HashSet::new();

    for glob_pattern in members {
        for member_path in expand_glob(root, glob_pattern) {
            let cargo_path = root_path.join(&member_path).join("Cargo.toml");
            if let Ok(content) = std::fs::read_to_string(&cargo_path)
                && let Ok(cargo) = toml::from_str::<CargoToml>(&content)
                && let Some(name) = cargo.package.as_ref().and_then(|p| p.name.as_deref())
            {
                member_names.insert(name.to_string());
            }
        }
    }

    let mut crates = vec![];
    for glob_pattern in members {
        for member_path in expand_glob(root, glob_pattern) {
            let cargo_path = root_path.join(&member_path).join("Cargo.toml");
            if let Ok(content) = std::fs::read_to_string(&cargo_path)
                && let Ok(cargo) = toml::from_str::<CargoToml>(&content)
            {
                let name = match cargo.package.as_ref().and_then(|p| p.name.as_deref()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                let is_binary = cargo.bin.as_ref().map(|b| !b.is_empty()).unwrap_or(false)
                    || root_path.join(&member_path).join("src/main.rs").exists();

                let depends_on = extract_workspace_deps(&cargo, &member_names);

                crates.push(CrateInfo {
                    name,
                    path: member_path,
                    is_binary,
                    depends_on,
                });
            }
        }
    }
    Ok(crates)
}

/// Expand a glob pattern relative to root (only handles trailing `*` patterns).
fn expand_glob(root: &str, pattern: &str) -> Vec<String> {
    let root_path = Path::new(root);
    if pattern.contains('*') {
        // e.g. "crates/*"
        let prefix = pattern.trim_end_matches('*').trim_end_matches('/');
        let dir = root_path.join(prefix);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            return entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| {
                    e.path()
                        .strip_prefix(root_path)
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

/// Extract workspace-member dependencies from a crate's Cargo.toml.
fn extract_workspace_deps(cargo: &CargoToml, member_names: &HashSet<String>) -> Vec<String> {
    let mut deps = vec![];
    if let Some(toml::Value::Table(table)) = &cargo.dependencies {
        for (dep_name, val) in table {
            // Check if it's a workspace dep (path = "..." or workspace = true pointing to a member)
            let is_member = member_names.contains(dep_name) && {
                match val {
                    toml::Value::Table(t) => {
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

// ---------------------------------------------------------------------------
// Topological sort (Kahn's algorithm)
// ---------------------------------------------------------------------------

fn topological_sort(crates: &[CrateInfo]) -> Vec<&CrateInfo> {
    let name_to_idx: HashMap<&str, usize> = crates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; crates.len()];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; crates.len()];

    for (i, c) in crates.iter().enumerate() {
        for dep in &c.depends_on {
            if let Some(&j) = name_to_idx.get(dep.as_str()) {
                // i depends on j → j must come before i
                adj[j].push(i);
                in_degree[i] += 1;
            }
        }
    }

    let mut queue: std::collections::VecDeque<usize> =
        (0..crates.len()).filter(|&i| in_degree[i] == 0).collect();
    let mut result = vec![];

    while let Some(node) = queue.pop_front() {
        result.push(node);
        for &next in &adj[node] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push_back(next);
            }
        }
    }

    // If we couldn't sort all (cycle), just append remaining in original order
    if result.len() < crates.len() {
        for i in 0..crates.len() {
            if !result.contains(&i) {
                result.push(i);
            }
        }
    }

    result.iter().map(|&i| &crates[i]).collect()
}

// ---------------------------------------------------------------------------
// YAML rendering
// ---------------------------------------------------------------------------

const COMMON_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

fn render_yaml(project_name: &str, crates: &[&CrateInfo]) -> Result<String> {
    let mut out = String::new();

    out.push_str(&format!("project_name: {}\n", project_name));
    out.push_str("dist: ./dist\n\n");

    out.push_str("defaults:\n");
    out.push_str("  targets:\n");
    for t in COMMON_TARGETS {
        out.push_str(&format!("    - {}\n", t));
    }
    out.push_str("  cross: auto\n\n");

    out.push_str("crates:\n");
    for c in crates {
        out.push_str(&format!("  - name: {}\n", c.name));
        out.push_str(&format!("    path: {}\n", c.path));
        out.push_str(&format!(
            "    tag_template: \"{}-v{{{{ .Version }}}}\"\n",
            c.name
        ));

        if let Some(deps) = non_empty_deps(&c.depends_on) {
            out.push_str("    depends_on:\n");
            for d in deps {
                out.push_str(&format!("      - {}\n", d));
            }
        }

        if c.is_binary {
            out.push_str("    builds:\n");
            out.push_str(&format!("      - binary: {}\n", c.name));
            out.push_str("    archives:\n");
            out.push_str(&format!(
                "      - name_template: \"{}-{{{{ .Version }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}\"\n",
                c.name
            ));
            out.push_str("    release:\n");
            out.push_str("      github:\n");
            out.push_str("        owner: YOUR_GITHUB_OWNER\n");
            out.push_str(&format!("        name: {}\n", project_name));
            out.push_str("      draft: false\n");
            out.push_str("      prerelease: auto\n");
        } else {
            out.push_str("    publish:\n");
            out.push_str("      crates: true\n");
        }
        out.push('\n');
    }

    Ok(out)
}

fn non_empty_deps(deps: &[String]) -> Option<&[String]> {
    if deps.is_empty() { None } else { Some(deps) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_single_crate_binary() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            r#"
[package]
name = "myapp"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "myapp"
path = "src/main.rs"
"#,
        );

        let yaml = generate_config(tmp.path().to_str().unwrap()).unwrap();
        assert!(yaml.contains("project_name: myapp"));
        assert!(yaml.contains("builds:"));
        assert!(yaml.contains("binary: myapp"));
        assert!(yaml.contains("archives:"));
        assert!(yaml.contains("release:"));
        // Should not have publish.crates since it's a binary
        assert!(!yaml.contains("crates: true"));
    }

    #[test]
    fn test_single_crate_library() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            r#"
[package]
name = "mylib"
version = "0.1.0"
edition = "2024"
"#,
        );

        let yaml = generate_config(tmp.path().to_str().unwrap()).unwrap();
        assert!(yaml.contains("project_name: mylib"));
        assert!(yaml.contains("crates: true"));
        // Library crates should not have builds
        assert!(!yaml.contains("builds:"));
    }

    #[test]
    fn test_workspace_with_mixed_crates() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            r#"
[workspace]
resolver = "2"
members = ["crates/mylib", "crates/mybin"]

[workspace.package]
name = "myproject"
version = "0.1.0"
edition = "2024"
"#,
        );
        write_file(
            tmp.path(),
            "crates/mylib/Cargo.toml",
            r#"
[package]
name = "mylib"
version = "0.1.0"
edition = "2024"
"#,
        );
        write_file(
            tmp.path(),
            "crates/mybin/Cargo.toml",
            r#"
[package]
name = "mybin"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "mybin"
path = "src/main.rs"

[dependencies]
mylib = { path = "../mylib" }
"#,
        );

        let yaml = generate_config(tmp.path().to_str().unwrap()).unwrap();
        assert!(yaml.contains("project_name: myproject"));
        // mylib is a library: should have publish.crates
        assert!(yaml.contains("crates: true"));
        // mybin is a binary: should have builds
        assert!(yaml.contains("builds:"));
        assert!(yaml.contains("binary: mybin"));
        // mybin depends on mylib
        assert!(yaml.contains("depends_on:"));
        assert!(yaml.contains("- mylib"));
        // mylib should come before mybin (topological order)
        let mylib_pos = yaml.find("name: mylib").unwrap();
        let mybin_pos = yaml.find("name: mybin").unwrap();
        assert!(mylib_pos < mybin_pos, "mylib should appear before mybin");
    }

    #[test]
    fn test_workspace_glob_expansion() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            r#"
[workspace]
resolver = "2"
members = ["crates/*"]
"#,
        );
        write_file(
            tmp.path(),
            "crates/alpha/Cargo.toml",
            r#"
[package]
name = "alpha"
version = "0.1.0"
edition = "2024"
"#,
        );
        write_file(
            tmp.path(),
            "crates/beta/Cargo.toml",
            r#"
[package]
name = "beta"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "beta"
path = "src/main.rs"
"#,
        );

        let yaml = generate_config(tmp.path().to_str().unwrap()).unwrap();
        assert!(yaml.contains("name: alpha"));
        assert!(yaml.contains("name: beta"));
        assert!(yaml.contains("binary: beta"));
    }
}
