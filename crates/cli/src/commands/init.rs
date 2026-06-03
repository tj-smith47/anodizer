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

// Empty struct -- only used to detect presence of [[bin]] entries in Cargo.toml
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
    let config_path = ".anodizer.yaml";
    if std::path::Path::new(config_path).exists() {
        anyhow::bail!("config file '{}' already exists", config_path);
    }

    let yaml = generate_config(".")?;
    std::fs::write(config_path, &yaml)
        .with_context(|| format!("failed to write {}", config_path))?;
    println!("Created {}", config_path);

    // Update .gitignore to include dist/
    let gitignore_path = ".gitignore";
    let gitignore = std::fs::read_to_string(gitignore_path).unwrap_or_default();
    if !gitignore.contains("dist/") {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(gitignore_path)
            .with_context(|| format!("failed to open {}", gitignore_path))?;
        use std::io::Write;
        if !gitignore.is_empty() && !gitignore.ends_with('\n') {
            writeln!(f)?;
        }
        writeln!(f, "dist/")?;
        println!("Added 'dist/' to {}", gitignore_path);
    }

    Ok(())
}

/// Generate anodizer.yaml content from a directory root.
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
    let items: Vec<(String, Vec<String>)> = crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone()))
        .collect();
    let sorted_names = anodizer_core::util::topological_sort(&items);

    let name_to_crate: HashMap<&str, &CrateInfo> =
        crates.iter().map(|c| (c.name.as_str(), c)).collect();

    sorted_names
        .iter()
        .filter_map(|name| name_to_crate.get(name.as_str()).copied())
        .collect()
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
            // Library crates opt in to crates.io publishing via `cargo: {}`.
            // Presence is the on-switch (no `enabled` field, no bool shorthand).
            out.push_str("    publish:\n");
            out.push_str("      cargo: {}\n");
        }
        out.push('\n');
    }

    Ok(out)
}

fn non_empty_deps(deps: &[String]) -> Option<&[String]> {
    if deps.is_empty() { None } else { Some(deps) }
}

// ---------------------------------------------------------------------------
// `init --version-files` — enrollment discovery + write-back
// ---------------------------------------------------------------------------

use anodizer_core::log::{StageLogger, Verbosity};

/// Repo-relative directory prefixes whose contents anodizer already version-syncs
/// or treats as build output, so enrolling them under `version_files` would
/// double-handle or churn on generated files.
const AUTO_EXCLUDED_PREFIXES: &[&str] = &["dist/", "target/", ".git/"];

/// Exact tracked paths anodizer already bumps (its `tag` command rewrites the
/// manifest and lockfile). The match is on the path's file name so a workspace
/// member's `crates/foo/Cargo.toml` is excluded as well as the root one.
const AUTO_EXCLUDED_FILENAMES: &[&str] = &["Cargo.toml", "Cargo.lock"];

/// Discover repo files that embed the current version and enroll the user's
/// selection into `version_files` in an existing `.anodizer.yaml`.
///
/// `exclude` are discovery-only globs dropped from the candidate set; `yes`
/// auto-selects every candidate (no prompt). The config write preserves the
/// file's existing comments and key order and is idempotent: already-enrolled
/// paths are never re-added.
pub fn enroll_version_files(
    exclude: Vec<String>,
    yes: bool,
    verbose: bool,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let log = StageLogger::new("init", Verbosity::from_flags(quiet, verbose, debug));

    let config_path = ".anodizer.yaml";
    if !Path::new(config_path).exists() {
        anyhow::bail!(
            "no '{config_path}' found — run `anodizer init` to scaffold one before enrolling version files"
        );
    }
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {config_path}"))?;

    let already_enrolled = existing_version_files(&config_text);
    let versions = scan_versions(Path::new("."))?;
    if versions.is_empty() {
        anyhow::bail!(
            "could not determine the current version to scan for (no readable [package].version or [workspace.package].version)"
        );
    }
    log.verbose(&format!("scanning for version(s): {}", versions.join(", ")));

    let tracked = anodizer_core::git::list_tracked_files_in(Path::new("."))
        .context("listing tracked files via `git ls-files`")?;

    let exclude_globs = compile_globs(&exclude)?;
    let candidates = discover_candidates(
        Path::new("."),
        &tracked,
        &versions,
        &already_enrolled,
        &exclude_globs,
    );

    if candidates.is_empty() {
        log.status("no un-enrolled files contain the current version — nothing to enroll");
        return Ok(());
    }

    let selected = if yes {
        candidates
    } else {
        select_interactive(&candidates)?
    };

    if selected.is_empty() {
        log.status("no files selected — nothing to enroll");
        return Ok(());
    }

    let (new_text, added) = add_version_files(&config_text, &selected);
    if added.is_empty() {
        log.status("all selected files were already enrolled — nothing to do");
        return Ok(());
    }

    std::fs::write(config_path, &new_text)
        .with_context(|| format!("failed to write {config_path}"))?;

    log.status(&format!(
        "enrolled {} file(s) under version_files in {config_path}",
        added.len()
    ));
    for path in &added {
        log.status(&format!("  + {path}"));
    }
    Ok(())
}

/// Collect the version string(s) to scan candidate files for, covering every
/// config mode: the shared `[workspace.package].version` (lockstep) plus each
/// member's own literal `[package].version` (per-crate), and the single-crate
/// root manifest as a fallback. De-duplicated; the engine matcher handles both
/// the bare and `v`-prefixed spelling of each.
fn scan_versions(root: &Path) -> Result<Vec<String>> {
    use crate::commands::bump::cargo_edit::load_workspace;
    use anodizer_stage_build::version_sync::read_cargo_version;

    let mut versions: Vec<String> = Vec::new();
    let mut push = |v: String| {
        // "0.0.0" is `read_cargo_version`'s sentinel for an inherited
        // (non-literal) version; the real value comes from the workspace entry.
        if v != "0.0.0" && !versions.contains(&v) {
            versions.push(v);
        }
    };

    if let Ok(ws) = load_workspace(root) {
        if let Some(v) = ws.workspace_package_version.clone() {
            push(v);
        }
        for member in &ws.members {
            if let Some(v) = member.own_version.clone() {
                push(v);
            }
        }
    }

    // Single-crate / rootless layout: the root manifest's own version.
    if let Ok(v) = read_cargo_version(&root.to_string_lossy()) {
        push(v);
    }

    Ok(versions)
}

/// Extract the paths already listed under a top-level `version_files:` block in
/// the raw config text, so discovery can drop them (idempotency) without a full
/// serde parse that would discard the user's comments. Recognizes the block
/// `version_files:` followed by `- <path>` list items at any indent; stops at
/// the next line that is not a list item or blank.
fn existing_version_files(config_text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_block = false;
    for line in config_text.lines() {
        let trimmed = line.trim_start();
        if !in_block {
            if line.trim_end() == "version_files:" || trimmed == "version_files:" {
                // Only a TOP-LEVEL block (no leading indent) is the fallback list
                // this flow writes to; a crate-scoped `version_files:` is nested.
                if line.starts_with("version_files:") {
                    in_block = true;
                }
            }
            continue;
        }
        if let Some(item) = parse_list_item(line) {
            out.insert(item);
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    out
}

/// Parse a YAML sequence item line (`- value` / `  - "value"`), returning the
/// unquoted scalar, or `None` if the line is not a list item.
fn parse_list_item(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("- ")?;
    let val = rest.trim();
    let unquoted = val
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(val);
    Some(unquoted.to_string())
}

/// Compile the user-supplied `--exclude` globs once.
fn compile_globs(patterns: &[String]) -> Result<Vec<glob::Pattern>> {
    patterns
        .iter()
        .map(|p| {
            glob::Pattern::new(p).with_context(|| format!("invalid --exclude glob pattern {p:?}"))
        })
        .collect()
}

/// Filter tracked files down to enrollment candidates: a UTF-8 text file that
/// contains one of `versions`, is not auto-excluded (manifest/lockfile, build
/// output), is not already enrolled, and survives the `--exclude` globs.
/// Binary / unreadable files are skipped silently. Results are sorted for a
/// stable prompt and deterministic tests.
fn discover_candidates(
    root: &Path,
    tracked: &[String],
    versions: &[String],
    already_enrolled: &HashSet<String>,
    exclude_globs: &[glob::Pattern],
) -> Vec<String> {
    let mut out: Vec<String> = tracked
        .iter()
        .filter(|p| !is_auto_excluded(p))
        .filter(|p| !already_enrolled.contains(*p))
        .filter(|p| !exclude_globs.iter().any(|g| g.matches(p)))
        .filter(|p| file_contains_any_version(&root.join(p), versions))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether a repo-relative path is auto-excluded from discovery (anodizer
/// already handles it, or it is build output).
fn is_auto_excluded(path: &str) -> bool {
    if AUTO_EXCLUDED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    AUTO_EXCLUDED_FILENAMES.contains(&name.as_str())
}

/// Read a file as UTF-8 and report whether it contains any of `versions`.
/// Non-UTF-8 / unreadable files (binaries) return `false` rather than erroring,
/// so one binary asset never fails the whole discovery run.
fn file_contains_any_version(path: &Path, versions: &[String]) -> bool {
    let content = match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    versions
        .iter()
        .any(|v| anodizer_core::version_files::contains_version(&content, v).unwrap_or(false))
}

/// Present a multi-select prompt (all candidates pre-checked) and return the
/// chosen subset. An Esc / cancel returns an empty selection. Status output
/// routes through the logger; the prompt itself is the interactive exception.
fn select_interactive(candidates: &[String]) -> Result<Vec<String>> {
    let defaults: Vec<bool> = vec![true; candidates.len()];
    let chosen = dialoguer::MultiSelect::new()
        .with_prompt("Select files to enroll under version_files (space toggles, enter confirms)")
        .items(candidates)
        .defaults(&defaults)
        .interact_opt()
        .context("version-files selection prompt failed")?;
    Ok(match chosen {
        Some(indices) => indices
            .into_iter()
            .filter_map(|i| candidates.get(i).cloned())
            .collect(),
        None => Vec::new(),
    })
}

/// Insert `selected` paths under the top-level `version_files:` block in
/// `config_text`, preserving all existing content. If the block is absent, a
/// well-formatted top-level block is appended; if present, only the
/// not-yet-listed items are inserted under it. Returns the rewritten text and
/// the paths actually added (empty when every selection was already enrolled).
fn add_version_files(config_text: &str, selected: &[String]) -> (String, Vec<String>) {
    let existing = existing_version_files(config_text);
    let mut to_add: Vec<String> = Vec::new();
    for path in selected {
        if !existing.contains(path) && !to_add.contains(path) {
            to_add.push(path.clone());
        }
    }
    if to_add.is_empty() {
        return (config_text.to_string(), to_add);
    }

    match find_version_files_block(config_text) {
        Some(insert_at) => {
            let mut lines: Vec<String> = config_text.lines().map(str::to_string).collect();
            let block = to_add.iter().map(|p| format!("  - {p}"));
            // Insert directly after the existing list so new items join the block.
            let tail = lines.split_off(insert_at);
            lines.extend(block);
            lines.extend(tail);
            let trailing_newline = config_text.ends_with('\n');
            let mut joined = lines.join("\n");
            if trailing_newline {
                joined.push('\n');
            }
            (joined, to_add)
        }
        None => {
            let mut out = config_text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            // Separate the appended block from preceding content with a blank line.
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("version_files:\n");
            for path in &to_add {
                out.push_str(&format!("  - {path}\n"));
            }
            (out, to_add)
        }
    }
}

/// Locate the line index immediately AFTER the last list item of an existing
/// top-level `version_files:` block, where new items should be inserted.
/// Returns `None` when no top-level block exists.
fn find_version_files_block(config_text: &str) -> Option<usize> {
    let lines: Vec<&str> = config_text.lines().collect();
    let mut start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with("version_files:") {
            start = Some(idx);
            break;
        }
    }
    let start = start?;
    // Walk past the list items belonging to the block.
    let mut last_item = start;
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if parse_list_item(line).is_some() {
            last_item = offset;
        } else if line.trim().is_empty() {
            continue;
        } else {
            break;
        }
    }
    Some(last_item + 1)
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
        // Should not have publish.cargo since it's a binary
        assert!(!yaml.contains("cargo: {}"));
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
        assert!(yaml.contains("cargo: {}"));
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
        // mylib is a library: should have publish.cargo
        assert!(yaml.contains("cargo: {}"));
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
