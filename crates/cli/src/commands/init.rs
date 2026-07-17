use anodizer_core::log::{StageLogger, Verbosity};
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
    let log = StageLogger::new("init", Verbosity::default());

    let config_path = ".anodizer.yaml";
    if std::path::Path::new(config_path).exists() {
        anyhow::bail!("config file '{}' already exists", config_path);
    }

    let yaml = generate_config(".")?;
    std::fs::write(config_path, &yaml)
        .with_context(|| format!("failed to write {}", config_path))?;
    log.status(&format!("Created {}", config_path));

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
        log.status(&format!("Added 'dist/' to {}", gitignore_path));
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

                let depends_on = anodizer_core::config::derive_depends_on_from_cargo_toml(
                    &root_path.join(&member_path),
                    &member_names,
                );

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
        // The generated template must mint tags in the same family the
        // no-template fallback scans, so compose it from the shared
        // fallback-prefix convention rather than restating `{name}-v`.
        out.push_str(&format!(
            "    tag_template: \"{}{{{{ .Version }}}}\"\n",
            anodizer_core::git::per_crate_tag_prefix(&c.name, "")
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

    let (new_text, added) = add_version_files(&config_text, &selected)?;
    if added.is_empty() {
        log.status("all selected files were already enrolled — nothing to do");
        return Ok(());
    }

    // Post-write guard: deserialize the rewritten text through the typed config
    // before touching disk, and confirm every newly enrolled path is present
    // under `version_files`. Any line-editor edge case that produced invalid or
    // wrong YAML fails here as a clean error rather than a corrupted config.
    validate_enrolled_yaml(&new_text, &added)
        .context("refusing to write .anodizer.yaml: the enrolled config did not validate")?;

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

    if let Ok(Some(ws)) = load_workspace(root) {
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

/// Extract the paths already listed under a top-level `version_files:` key in
/// the raw config text, so discovery can drop them (idempotency) without a full
/// serde parse that would discard the user's comments. Handles BOTH spellings:
///   * block style — `version_files:` followed by `- <path>` items at any
///     indent (stops at the next non-item, non-blank line);
///   * flow style — `version_files: [a.md, b.md]` inline on one line.
fn existing_version_files(config_text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_block = false;
    for line in config_text.lines() {
        let trimmed = line.trim_start();
        if !in_block {
            // A top-level key only (no leading indent) is the fallback list this
            // flow writes to; a crate-scoped `version_files:` is nested/indented.
            if let Some(inline) = line.strip_prefix("version_files:") {
                let inline = inline.trim();
                if inline.is_empty() {
                    // Block style: items follow on subsequent lines.
                    in_block = true;
                } else {
                    // Flow style (or a scalar): parse the inline value's members.
                    for item in parse_flow_members(inline) {
                        out.insert(item);
                    }
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

/// Whether the top-level `version_files:` value is written FLOW style (an inline
/// `[...]` sequence) rather than a block list. The format-preserving block
/// writer can only safely append `- item` lines under a block key, so a flow
/// list is detected and rejected with an actionable error instead of corrupting
/// the file. Returns `false` when there is no top-level `version_files:` key, or
/// it is the empty-value block-style header.
fn version_files_is_flow_style(config_text: &str) -> bool {
    config_text.lines().any(|line| {
        line.strip_prefix("version_files:")
            .map(|rest| rest.trim().starts_with('['))
            .unwrap_or(false)
    })
}

/// Parse the comma-separated members of an inline YAML flow sequence
/// (`[a.md, "b c.md"]`), stripping the surrounding brackets and per-item quotes.
/// A bare inline scalar (no brackets) yields that single unquoted value.
fn parse_flow_members(inline: &str) -> Vec<String> {
    let inner = inline
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inline);
    inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(unquote_scalar)
        .collect()
}

/// Strip a single layer of matching surrounding `"` or `'` quotes from a YAML
/// scalar, returning the inner text; an unquoted scalar is returned as-is.
fn unquote_scalar(val: &str) -> String {
    val.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(val)
        .to_string()
}

/// Parse a YAML sequence item line (`- value` / `  - "value"`), returning the
/// unquoted scalar, or `None` if the line is not a list item.
fn parse_list_item(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("- ")?;
    Some(unquote_scalar(rest.trim()))
}

/// Render a repo-relative path as a YAML scalar, double-quoting (and escaping)
/// it when it is not a plain scalar. A path with a space, a YAML indicator
/// (`:`, `#`, `[`, `{`, `*`, `&`, `!`, `|`, `>`, `@`, `` ` ``, `%`, `,`, quotes)
/// or a leading dash/question-mark would otherwise be mis-parsed or invalidate
/// the document; quoting keeps the enrolled string byte-identical to the path
/// the rewrite pass reads back. `parse_list_item` strips the quotes on
/// re-discovery, so a quoted entry still round-trips for idempotency.
fn yaml_scalar(path: &str) -> String {
    if is_plain_yaml_scalar(path) {
        path.to_string()
    } else {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

/// Whether `s` is safe to emit as an unquoted (plain) YAML scalar in flow/block
/// context. Conservative: any whitespace, leading indicator, or in-line
/// indicator that YAML treats specially forces quoting.
fn is_plain_yaml_scalar(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Leading indicators that start a non-scalar node or are otherwise unsafe
    // at the head of a plain scalar.
    let first = s.chars().next().unwrap_or(' ');
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    // Any whitespace, or a `:`/`#` that YAML would read as a mapping / comment
    // indicator mid-token, forces quoting.
    !s.chars()
        .any(|c| c.is_whitespace() || c == ':' || c == '#' || c == '\t')
}

/// Match options for `--exclude` globs: `require_literal_separator` scopes a
/// single `*` to one path segment (so `*.md` does NOT cross `/`, matching
/// gitignore / shell intuition); `**` remains the cross-`/` wildcard.
const EXCLUDE_MATCH_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

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
        .filter(|p| {
            !exclude_globs
                .iter()
                .any(|g| g.matches_with(p, EXCLUDE_MATCH_OPTIONS))
        })
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

/// Default sequence-item indent for a freshly written `version_files:` block.
const DEFAULT_BLOCK_INDENT: &str = "  ";

/// Where new items go and at what indent, when a top-level block already exists.
struct BlockInsertion {
    /// Line index immediately after the block's last list item.
    insert_at: usize,
    /// Leading-whitespace string of the existing items, matched on new items.
    indent: String,
}

/// Insert `selected` paths under the top-level `version_files:` block in
/// `config_text`, preserving all existing content. If the key is absent, a
/// well-formatted top-level block is appended; if a block exists, only the
/// not-yet-listed items are inserted under it at the block's own indent. Items
/// are emitted as YAML scalars (quoted when special) so a path with a space or
/// indicator can never invalidate the document.
///
/// Returns the rewritten text and the paths actually added (empty when every
/// selection was already enrolled). Bails when the existing `version_files:`
/// value is FLOW style (`[...]`), which the block writer cannot extend without
/// corrupting the document.
fn add_version_files(config_text: &str, selected: &[String]) -> Result<(String, Vec<String>)> {
    if version_files_is_flow_style(config_text) {
        anyhow::bail!(
            "`version_files` is written as an inline (flow) list in .anodizer.yaml; \
             rewrite it as a block list (one `- path` per line under `version_files:`) \
             to enroll automatically"
        );
    }

    let existing = existing_version_files(config_text);
    let mut to_add: Vec<String> = Vec::new();
    for path in selected {
        if !existing.contains(path) && !to_add.contains(path) {
            to_add.push(path.clone());
        }
    }
    if to_add.is_empty() {
        return Ok((config_text.to_string(), to_add));
    }

    let new_text = match find_version_files_block(config_text) {
        Some(BlockInsertion { insert_at, indent }) => {
            let mut lines: Vec<String> = config_text.lines().map(str::to_string).collect();
            let block = to_add
                .iter()
                .map(|p| format!("{indent}- {}", yaml_scalar(p)));
            // Insert directly after the existing list so new items join the block.
            let tail = lines.split_off(insert_at);
            lines.extend(block);
            lines.extend(tail);
            let trailing_newline = config_text.ends_with('\n');
            let mut joined = lines.join("\n");
            if trailing_newline {
                joined.push('\n');
            }
            joined
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
                out.push_str(&format!("{DEFAULT_BLOCK_INDENT}- {}\n", yaml_scalar(path)));
            }
            out
        }
    };

    Ok((new_text, to_add))
}

/// Locate the insertion point (and item indent) of an existing top-level
/// `version_files:` block. Returns `None` when no top-level block exists. The
/// indent is taken from the block's first list item so new items align with the
/// existing ones (handling a 4-space block as readily as a 2-space one); it
/// falls back to the default when the block header has no items yet.
fn find_version_files_block(config_text: &str) -> Option<BlockInsertion> {
    let lines: Vec<&str> = config_text.lines().collect();
    let mut start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        // A top-level (unindented) `version_files:` key with no inline value.
        if let Some(rest) = line.strip_prefix("version_files:")
            && rest.trim().is_empty()
        {
            start = Some(idx);
            break;
        }
    }
    let start = start?;
    // Walk past the list items belonging to the block, capturing the first
    // item's indent.
    let mut last_item = start;
    let mut indent: Option<String> = None;
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if parse_list_item(line).is_some() {
            last_item = offset;
            if indent.is_none() {
                let lead: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                indent = Some(lead);
            }
        } else if line.trim().is_empty() {
            continue;
        } else {
            break;
        }
    }
    Some(BlockInsertion {
        insert_at: last_item + 1,
        indent: indent.unwrap_or_else(|| DEFAULT_BLOCK_INDENT.to_string()),
    })
}

/// Deserialize `new_text` through the typed [`Config`] and confirm every path in
/// `added` is present in the top-level `version_files`. This is the safety net
/// behind the line-based writer: if the edit produced YAML that does not parse
/// (or parses but dropped an enrolled path), this errors so the caller can bail
/// WITHOUT writing — an invalid `.anodizer.yaml` can never ship.
fn validate_enrolled_yaml(new_text: &str, added: &[String]) -> Result<()> {
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(new_text)
        .context("rewritten config is not valid YAML / config schema")?;
    let enrolled = config.version_files.unwrap_or_default();
    for path in added {
        if !enrolled.contains(path) {
            anyhow::bail!("enrolled path {path:?} is missing from version_files after the edit");
        }
    }
    Ok(())
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
    fn test_workspace_depends_on_includes_build_dependency_only_member() {
        // A workspace member reachable ONLY via [build-dependencies] is an
        // equally hard publish-order requirement (cargo resolves build-deps
        // for `cargo publish` verification too) — init's scaffolded
        // depends_on must not silently drop it, matching config-load's
        // derive_depends_on_from_cargo_toml (the complete [dependencies] +
        // [build-dependencies] + [target.*.dependencies] union).
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            r#"
[workspace]
resolver = "2"
members = ["crates/build-helper", "crates/mybin"]

[workspace.package]
name = "myproject"
version = "0.1.0"
edition = "2024"
"#,
        );
        write_file(
            tmp.path(),
            "crates/build-helper/Cargo.toml",
            r#"
[package]
name = "build-helper"
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

[build-dependencies]
build-helper = { path = "../build-helper" }
"#,
        );

        let yaml = generate_config(tmp.path().to_str().unwrap()).unwrap();
        assert!(yaml.contains("depends_on:"));
        assert!(yaml.contains("- build-helper"));
        let helper_pos = yaml.find("name: build-helper").unwrap();
        let mybin_pos = yaml.find("name: mybin").unwrap();
        assert!(
            helper_pos < mybin_pos,
            "build-helper should appear before mybin (topological order)"
        );
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

    // -----------------------------------------------------------------------
    // version_files enrollment write-back
    // -----------------------------------------------------------------------

    #[test]
    fn existing_version_files_parses_block_and_flow() {
        let block = "project_name: app\nversion_files:\n  - a.md\n  - \"b c.md\"\n";
        let got = existing_version_files(block);
        assert!(got.contains("a.md"));
        assert!(got.contains("b c.md"));

        let flow = "project_name: app\nversion_files: [a.md, \"b c.md\"]\n";
        let got = existing_version_files(flow);
        assert!(got.contains("a.md"), "flow members not parsed: {got:?}");
        assert!(got.contains("b c.md"), "flow members not parsed: {got:?}");
    }

    #[test]
    fn add_version_files_bails_on_flow_style() {
        let cfg = "project_name: app\nversion_files: [a.md]\n";
        let err = add_version_files(cfg, &["b.md".to_string()]).unwrap_err();
        assert!(err.to_string().contains("inline"), "err: {err}");
        assert!(err.to_string().contains("block list"), "err: {err}");
    }

    #[test]
    fn add_version_files_matches_four_space_indent() {
        let cfg = "project_name: app\nversion_files:\n    - a.md\n";
        let (out, added) = add_version_files(cfg, &["b.md".to_string()]).unwrap();
        assert_eq!(added, vec!["b.md".to_string()]);
        assert!(out.contains("    - b.md"), "indent not matched:\n{out}");
        // Parses as valid YAML with both entries.
        let cfg: anodizer_core::config::Config = serde_yaml_ng::from_str(&out).unwrap();
        let vf = cfg.version_files.unwrap();
        assert!(vf.contains(&"a.md".to_string()));
        assert!(vf.contains(&"b.md".to_string()));
    }

    #[test]
    fn add_version_files_appends_block_when_absent() {
        let cfg = "project_name: app\n";
        let (out, _) = add_version_files(cfg, &["a.md".to_string()]).unwrap();
        assert!(out.contains("version_files:\n  - a.md\n"), "out:\n{out}");
    }

    #[test]
    fn yaml_scalar_quotes_special_paths() {
        assert_eq!(yaml_scalar("plain/path.md"), "plain/path.md");
        assert_eq!(yaml_scalar("with space.md"), "\"with space.md\"");
        assert_eq!(yaml_scalar("a:b.md"), "\"a:b.md\"");
        assert_eq!(yaml_scalar("# leading.md"), "\"# leading.md\"");
        assert_eq!(yaml_scalar("[bracket].md"), "\"[bracket].md\"");
        // A quoted spaced path round-trips through parse_list_item unquoting.
        let line = format!("  - {}", yaml_scalar("with space.md"));
        assert_eq!(parse_list_item(&line).as_deref(), Some("with space.md"));
    }

    #[test]
    fn add_version_files_quotes_spaced_path_and_validates() {
        let cfg = "project_name: app\n";
        let (out, added) = add_version_files(cfg, &["with space.md".to_string()]).unwrap();
        assert!(out.contains("- \"with space.md\""), "not quoted:\n{out}");
        validate_enrolled_yaml(&out, &added).unwrap();
    }

    #[test]
    fn validate_enrolled_yaml_rejects_invalid() {
        // A bogus top-level key under deny_unknown_fields fails to deserialize.
        let bad = "project_name: app\nnot_a_real_key: true\n";
        assert!(validate_enrolled_yaml(bad, &[]).is_err());
    }
}
