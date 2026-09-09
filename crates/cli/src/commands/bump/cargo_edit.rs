//! Cargo.toml reading + editing for `anodizer bump`.
//!
//! Uses `toml_edit` to preserve formatting and comments. Responsibilities:
//!   - Load the workspace graph (root Cargo.toml + member manifests).
//!   - Rewrite a member's `[package].version` (or the root
//!     `[workspace.package].version` if the member inherits).
//!   - Rewrite sibling `[dependencies]` / `[dev-dependencies]` /
//!     `[build-dependencies]` entries that reference a bumped member
//!     by its new version (unless `--exact` is set).
//!   - Heal every internal `path + version` dependency floor in the workspace
//!     against the path crate's post-bump version, not only the crates bumped
//!     in this run.

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
/// Load the Cargo workspace graph rooted at `workspace_root`.
///
/// Returns `Ok(None)` when there is genuinely no Cargo workspace — no root
/// `Cargo.toml` at all (anodizer also releases non-Rust / prebuilt-artifact
/// projects, which legitimately have no manifest). That absent case is
/// distinct from a `Cargo.toml` that EXISTS but fails to read or parse, or a
/// member manifest / member glob that errors: those are real defects and
/// surface as `Err`. Callers must NOT flatten the `Err` into "no workspace"
/// (the historical `.ok()` bug): doing so let a malformed manifest silently
/// abandon the lockstep path and resolve the version off a wrong base, cutting
/// the wrong tag while reporting success.
pub fn load_workspace(workspace_root: &Path) -> Result<Option<WorkspaceInfo>> {
    let root_manifest = workspace_root.join("Cargo.toml");
    if !root_manifest.exists() {
        return Ok(None);
    }
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

    Ok(Some(WorkspaceInfo {
        members,
        workspace_package_version,
    }))
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
    let ws = load_workspace(workspace_root)?
        .with_context(|| format!("no Cargo workspace at {}", workspace_root.display()))?;
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
        heal_dep_floors(workspace_root, &bumped, false, log)?;
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
                        "rewrote {} {} = \"{}\" in {}",
                        section,
                        dep_name,
                        new_ver,
                        manifest_path.display()
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
                                    "rewrote target.{} {} = \"{}\" in {}",
                                    section,
                                    dep_name,
                                    new_ver,
                                    manifest_path.display()
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
                "updated root [workspace.dependencies] {} = \"{}\"",
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

/// The version a workspace member carries: its own literal `[package].version`,
/// or the root `[workspace.package].version` when it inherits.
pub(crate) fn member_version(m: &MemberInfo, ws: &WorkspaceInfo) -> Option<String> {
    if m.inherits_workspace_version {
        ws.workspace_package_version.clone()
    } else {
        m.own_version.clone()
    }
}

/// The dependency sections a manifest may declare, both at the top level and
/// under a `[target.<cfg>]` table.
const DEP_SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The version requirement a dependency entry declares, across the bare-string,
/// inline-table, and sub-table shapes.
fn dep_version_spec(item: &Item) -> Option<&str> {
    match item {
        Item::Value(Value::String(s)) => Some(s.value()),
        _ => item.get("version").and_then(|v| v.as_str()),
    }
}

/// Whether a comparator bounds its requirement from below, i.e. whether the
/// requirement admits a lowest version at all.
fn is_lower_bounded(op: semver::Op) -> bool {
    matches!(
        op,
        semver::Op::Exact
            | semver::Op::Greater
            | semver::Op::GreaterEq
            | semver::Op::Tilde
            | semver::Op::Caret
    )
}

/// The lowest version a single-comparator requirement admits, or `None` when the
/// requirement has no lower bound (`<`, `<=`, `*`), is unparseable, or carries
/// more than one comparator.
fn floor_minimum(spec: &str) -> Option<semver::Version> {
    let req = semver::VersionReq::parse(spec).ok()?;
    let [c] = req.comparators.as_slice() else {
        return None;
    };
    if !is_lower_bounded(c.op) {
        return None;
    }
    Some(semver::Version {
        major: c.major,
        minor: c.minor.unwrap_or(0),
        patch: c.patch.unwrap_or(0),
        pre: c.pre.clone(),
        build: semver::BuildMetadata::EMPTY,
    })
}

/// Re-render `spec` at `new`, preserving the comparator operator and the
/// component precision the author wrote (`"0.6"` → `"0.7"`, `"^0.6.1"` →
/// `"^0.7.0"`, `"=0.6.1"` → `"=0.7.0"`).
///
/// Returns `None` for a requirement `floor_minimum` also rejects.
fn restyle_floor(spec: &str, new: &semver::Version) -> Option<String> {
    let req = semver::VersionReq::parse(spec).ok()?;
    let [c] = req.comparators.as_slice() else {
        return None;
    };
    if !is_lower_bounded(c.op) {
        return None;
    }
    // Everything the author wrote before the first digit is the operator,
    // spacing included, so `">= 0.6.1"` keeps its space.
    let op = &spec.trim()[..spec.trim().find(|ch: char| ch.is_ascii_digit())?];
    // A comparator that carries no pre-release identifier never matches a
    // pre-release version, so a pre-release target renders at full precision.
    if !new.pre.is_empty() {
        return Some(format!(
            "{op}{}.{}.{}-{}",
            new.major, new.minor, new.patch, new.pre
        ));
    }
    Some(match (c.minor, c.patch) {
        (None, _) => format!("{op}{}", new.major),
        (Some(_), None) => format!("{op}{}.{}", new.major, new.minor),
        _ => format!("{op}{}.{}.{}", new.major, new.minor, new.patch),
    })
}

/// Everything one manifest's dep tables need to decide and report a heal.
struct HealScope<'a> {
    resolved: &'a BTreeMap<String, semver::Version>,
    dirs: &'a BTreeMap<String, PathBuf>,
    manifest_dir: PathBuf,
    manifest_rel: String,
    dry_run: bool,
}

/// Raise every stale internal floor in one dependency table. Returns whether
/// anything was rewritten.
fn heal_dep_table(tbl: &mut toml_edit::Table, scope: &HealScope<'_>, log: &StageLogger) -> bool {
    // Decide against the immutable table first: `rewrite_dep_entry` needs the
    // table mutably, and the decision needs to read every entry.
    let mut writes: Vec<(String, String, String, String)> = Vec::new();
    for (key, item) in tbl.iter() {
        let dep_name = item.get("package").and_then(|p| p.as_str()).unwrap_or(key);
        let (Some(want), Some(dir)) = (scope.resolved.get(dep_name), scope.dirs.get(dep_name))
        else {
            continue;
        };
        let Some(path) = item.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        // A registry dep may share a member's name; only a `path` that lands on
        // that member's directory is an internal floor.
        if std::fs::canonicalize(scope.manifest_dir.join(path))
            .ok()
            .as_ref()
            != Some(dir)
        {
            continue;
        }
        let Some(spec) = dep_version_spec(item) else {
            continue;
        };
        let Some(floor) = floor_minimum(spec) else {
            match semver::VersionReq::parse(spec) {
                Ok(req) if req.comparators.len() > 1 => log.warn(&format!(
                    "multi-comparator version requirement {dep_name} = \"{spec}\" in {}; dep floor left unchanged",
                    scope.manifest_rel
                )),
                Err(_) => log.warn(&format!(
                    "unparseable version requirement {dep_name} = \"{spec}\" in {}; dep floor left unchanged",
                    scope.manifest_rel
                )),
                _ => {}
            }
            continue;
        };
        if floor >= *want {
            continue;
        }
        let Some(next) = restyle_floor(spec, want) else {
            continue;
        };
        if next == spec {
            continue;
        }
        writes.push((
            key.to_string(),
            dep_name.to_string(),
            spec.to_string(),
            next,
        ));
    }

    let mut changed = false;
    for (key, dep_name, old, next) in writes {
        if !rewrite_dep_entry(tbl, &key, &next) {
            continue;
        }
        if scope.dry_run {
            log.status(&format!(
                "(dry-run) would heal dep floor {dep_name} {old} → {next} in {}",
                scope.manifest_rel
            ));
        } else {
            log.status(&format!(
                "healed dep floor {dep_name} {old} → {next} in {}",
                scope.manifest_rel
            ));
        }
        changed = true;
    }
    changed
}

/// Raise every internal `path + version` dependency floor in the Cargo
/// workspace rooted at `scope_root` to at least the path crate's post-bump
/// version.
///
/// A floor lower than the path crate's version is always wrong under
/// `path + version` semantics — cargo requires the path crate to satisfy the
/// floor at publish time — so it is rewritten regardless of whether that crate
/// was bumped in this run. A prior bump commit that never reached the default
/// branch therefore heals on the next bump instead of poisoning its publish.
///
/// `pending` maps crate name → the version this run is bumping it to; a member
/// absent from it resolves its version from its own manifest (or the root
/// `[workspace.package].version` when it inherits). Floors already at or above
/// the resolved version are left byte-for-byte untouched, as are floors whose
/// requirement is not a single lower-bounded comparator.
///
/// Returns the absolute paths of the manifests that changed, for staging into
/// the bump commit. Writes nothing when `dry_run` is set.
pub(crate) fn heal_dep_floors(
    scope_root: &Path,
    pending: &BTreeMap<String, String>,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<PathBuf>> {
    let Some(ws) = load_workspace(scope_root)? else {
        return Ok(Vec::new());
    };
    let mut resolved: BTreeMap<String, semver::Version> = BTreeMap::new();
    let mut dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    for m in &ws.members {
        let Some(Ok(version)) = pending
            .get(&m.name)
            .cloned()
            .or_else(|| member_version(m, &ws))
            .map(|v| semver::Version::parse(&v))
        else {
            continue;
        };
        resolved.insert(m.name.clone(), version);
        dirs.insert(
            m.name.clone(),
            std::fs::canonicalize(&m.crate_dir).unwrap_or_else(|_| m.crate_dir.clone()),
        );
    }
    if resolved.is_empty() {
        return Ok(Vec::new());
    }

    let mut manifests: Vec<PathBuf> = vec![scope_root.join("Cargo.toml")];
    for m in &ws.members {
        if !manifests.contains(&m.manifest_path) {
            manifests.push(m.manifest_path.clone());
        }
    }

    let mut healed: Vec<PathBuf> = Vec::new();
    for manifest in manifests {
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("failed to read {}", manifest.display()))?;
        let mut doc = text
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse {}", manifest.display()))?;
        let scope = HealScope {
            resolved: &resolved,
            dirs: &dirs,
            manifest_dir: manifest
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| scope_root.to_path_buf()),
            manifest_rel: manifest
                .strip_prefix(scope_root)
                .unwrap_or(manifest.as_path())
                .display()
                .to_string(),
            dry_run,
        };

        let mut changed = false;
        for section in DEP_SECTIONS {
            if let Some(tbl) = doc.get_mut(section).and_then(|i| i.as_table_mut()) {
                changed |= heal_dep_table(tbl, &scope, log);
            }
        }
        if let Some(target) = doc.get_mut("target").and_then(|i| i.as_table_mut()) {
            for (_, item) in target.iter_mut() {
                let Some(tt) = item.as_table_mut() else {
                    continue;
                };
                for section in DEP_SECTIONS {
                    if let Some(tbl) = tt.get_mut(section).and_then(|i| i.as_table_mut()) {
                        changed |= heal_dep_table(tbl, &scope, log);
                    }
                }
            }
        }
        if let Some(tbl) = doc
            .get_mut("workspace")
            .and_then(|w| w.as_table_mut())
            .and_then(|w| w.get_mut("dependencies"))
            .and_then(|d| d.as_table_mut())
        {
            changed |= heal_dep_table(tbl, &scope, log);
        }

        if !changed {
            continue;
        }
        if !dry_run {
            std::fs::write(&manifest, doc.to_string())
                .with_context(|| format!("failed to write {}", manifest.display()))?;
        }
        healed.push(manifest);
    }
    Ok(healed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn quiet_log() -> StageLogger {
        StageLogger::new("tag", anodizer_core::log::Verbosity::Quiet)
    }

    /// Write a workspace root plus `crates/<name>/Cargo.toml` for each member.
    fn write_workspace(dir: &Path, root: &str, members: &[(&str, &str)]) {
        std::fs::write(dir.join("Cargo.toml"), root).unwrap();
        for (name, body) in members {
            let crate_dir = dir.join("crates").join(name);
            std::fs::create_dir_all(&crate_dir).unwrap();
            std::fs::write(crate_dir.join("Cargo.toml"), body).unwrap();
        }
    }

    fn pkg(name: &str, version: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n")
    }

    fn ws_root(members: &[&str]) -> String {
        let list = members
            .iter()
            .map(|m| format!("\"crates/{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[workspace]\nmembers = [{list}]\nresolver = \"2\"\n")
    }

    fn no_pending() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn ver(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn floor_minimum_reads_each_operator_form() {
        assert_eq!(floor_minimum("0.6"), Some(ver("0.6.0")));
        assert_eq!(floor_minimum("0.6.1"), Some(ver("0.6.1")));
        assert_eq!(floor_minimum("^0.6.1"), Some(ver("0.6.1")));
        assert_eq!(floor_minimum("~0.6.1"), Some(ver("0.6.1")));
        assert_eq!(floor_minimum("=0.6.1"), Some(ver("0.6.1")));
        assert_eq!(floor_minimum(">=0.6.1"), Some(ver("0.6.1")));
        assert_eq!(floor_minimum("<0.9"), None);
        assert_eq!(floor_minimum("*"), None);
        assert_eq!(floor_minimum(">=0.6, <0.8"), None);
        assert_eq!(floor_minimum("junk"), None);
    }

    #[test]
    fn restyle_floor_preserves_operator_and_precision() {
        let target = ver("0.7.0");
        assert_eq!(restyle_floor("0.6", &target).as_deref(), Some("0.7"));
        assert_eq!(restyle_floor("0.6.1", &target).as_deref(), Some("0.7.0"));
        assert_eq!(restyle_floor("^0.6.1", &target).as_deref(), Some("^0.7.0"));
        assert_eq!(restyle_floor("~0.6", &target).as_deref(), Some("~0.7"));
        assert_eq!(restyle_floor("=0.6.1", &target).as_deref(), Some("=0.7.0"));
        assert_eq!(
            restyle_floor(">=0.6.1", &target).as_deref(),
            Some(">=0.7.0")
        );
        // Two-component precision is kept even when the residual minimum sits
        // below the target patch: the requirement is still satisfied.
        assert_eq!(restyle_floor("0.6", &ver("0.7.1")).as_deref(), Some("0.7"));
        // Identical rendering — the caller writes nothing.
        assert_eq!(restyle_floor("0", &target).as_deref(), Some("0"));
        assert_eq!(
            restyle_floor("^0.6.1", &ver("0.7.0-rc.1")).as_deref(),
            Some("^0.7.0-rc.1")
        );
        assert_eq!(restyle_floor("<0.9", &target), None);
        assert_eq!(restyle_floor("junk", &target), None);
    }

    #[test]
    fn heal_dep_floors_raises_stale_floor_on_non_bumped_dependent() {
        let dir = tmpdir();
        write_workspace(
            dir.path(),
            &ws_root(&["a", "b", "c"]),
            &[
                ("a", &pkg("a", "0.1.0")),
                ("b", &pkg("b", "0.7.0")),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        let healed = heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        assert_eq!(healed, vec![dir.path().join("crates/c/Cargo.toml")]);
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(c.contains("version = \"0.7.0\""), "{c}");
    }

    #[test]
    fn heal_dep_floors_leaves_satisfied_floor_byte_identical() {
        let dir = tmpdir();
        let c_manifest = format!(
            "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.7.0\" }}\n\n[dev-dependencies]\nb2 = {{ package = \"b\", path = \"../b\", version = \"0.9\" }}\n",
            pkg("c", "0.1.0")
        );
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        let healed = heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        assert!(healed.is_empty(), "{healed:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            c_manifest
        );
    }

    #[test]
    fn heal_dep_floors_preserves_caret_and_short_forms() {
        let dir = tmpdir();
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[
                ("b", &pkg("b", "0.7.0")),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"^0.6.1\" }}\n\n[build-dependencies]\nshort = {{ package = \"b\", path = \"../b\", version = \"0.6\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(c.contains("version = \"^0.7.0\""), "{c}");
        assert!(c.contains("version = \"0.7\""), "{c}");
    }

    #[test]
    fn heal_dep_floors_heals_exact_pin_to_new_version() {
        let dir = tmpdir();
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[
                ("b", &pkg("b", "0.7.0")),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"=0.6.1\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(c.contains("version = \"=0.7.0\""), "{c}");
    }

    #[test]
    fn heal_dep_floors_walks_every_dep_table() {
        let dir = tmpdir();
        let root = format!(
            "{}\n[workspace.dependencies]\nb = {{ path = \"crates/b\", version = \"0.6.1\" }}\n",
            ws_root(&["b", "c"])
        );
        let c_manifest = format!(
            "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n\n[dev-dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n\n[build-dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n\n[target.'cfg(windows)'.dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n",
            pkg("c", "0.1.0")
        );
        write_workspace(
            dir.path(),
            &root,
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        let healed = heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        assert_eq!(healed.len(), 2, "{healed:?}");
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert_eq!(
            c.matches("version = \"0.7.0\"").count(),
            4,
            "all four member tables must heal: {c}"
        );
        let root_after = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(root_after.contains("version = \"0.7.0\""), "{root_after}");
    }

    #[test]
    fn heal_dep_floors_ignores_registry_dep_without_path() {
        let dir = tmpdir();
        let c_manifest = format!("{}\n[dependencies]\nb = \"0.6.1\"\n", pkg("c", "0.1.0"));
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        assert!(
            heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            c_manifest
        );
    }

    #[test]
    fn heal_dep_floors_ignores_path_dep_outside_workspace() {
        let dir = tmpdir();
        let outside = dir.path().join("elsewhere/b");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Cargo.toml"), pkg("b", "0.1.0")).unwrap();
        let c_manifest = format!(
            "{}\n[dependencies]\nb = {{ path = \"../../elsewhere/b\", version = \"0.1.0\" }}\n",
            pkg("c", "0.1.0")
        );
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        assert!(
            heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            c_manifest
        );
    }

    #[test]
    fn heal_dep_floors_resolves_inheriting_member_version() {
        let dir = tmpdir();
        let root = format!(
            "{}\n[workspace.package]\nversion = \"0.9.0\"\n",
            ws_root(&["b", "c"])
        );
        write_workspace(
            dir.path(),
            &root,
            &[
                ("b", "[package]\nname = \"b\"\nversion.workspace = true\n"),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.6.1\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(c.contains("version = \"0.9.0\""), "{c}");
    }

    #[test]
    fn heal_dep_floors_honors_renamed_package_key() {
        let dir = tmpdir();
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[
                ("b", &pkg("b", "0.7.0")),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nalias = {{ package = \"b\", path = \"../b\", version = \"0.6.1\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log()).unwrap();
        let c = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(c.contains("alias = { package = \"b\""), "{c}");
        assert!(c.contains("version = \"0.7.0\""), "{c}");
    }

    #[test]
    fn heal_dep_floors_pending_overrides_disk_under_dry_run() {
        let dir = tmpdir();
        let c_manifest = format!(
            "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.7.0\" }}\n",
            pkg("c", "0.1.0")
        );
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        let pending: BTreeMap<String, String> = [("b".to_string(), "0.8.0".to_string())]
            .into_iter()
            .collect();
        let healed = heal_dep_floors(dir.path(), &pending, true, &quiet_log()).unwrap();
        assert_eq!(healed, vec![dir.path().join("crates/c/Cargo.toml")]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            c_manifest,
            "dry-run must write nothing"
        );
    }

    #[test]
    fn heal_dep_floors_is_idempotent() {
        let dir = tmpdir();
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[
                ("b", &pkg("b", "0.7.1")),
                (
                    "c",
                    &format!(
                        "{}\n[dependencies]\nb = {{ path = \"../b\", version = \"0.6\" }}\n",
                        pkg("c", "0.1.0")
                    ),
                ),
            ],
        );
        assert_eq!(
            heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log())
                .unwrap()
                .len(),
            1
        );
        let after_first = std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap();
        assert!(after_first.contains("version = \"0.7\""), "{after_first}");
        assert!(
            heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            after_first
        );
    }

    #[test]
    fn heal_dep_floors_warns_on_multi_comparator_floor() {
        let dir = tmpdir();
        let c_manifest = format!(
            "{}\n[dependencies]\nb = {{ path = \"../b\", version = \">=0.6, <0.8\" }}\n",
            pkg("c", "0.1.0")
        );
        write_workspace(
            dir.path(),
            &ws_root(&["b", "c"]),
            &[("b", &pkg("b", "0.7.0")), ("c", &c_manifest)],
        );
        assert!(
            heal_dep_floors(dir.path(), &no_pending(), false, &quiet_log())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crates/c/Cargo.toml")).unwrap(),
            c_manifest
        );
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
        let ws = load_workspace(dir.path())
            .unwrap()
            .expect("a workspace with a root Cargo.toml is present");
        assert_eq!(ws.workspace_package_version.as_deref(), Some("0.1.0"));
        let names: Vec<&str> = ws.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(ws.members[0].inherits_workspace_version);
        assert_eq!(ws.members[1].own_version.as_deref(), Some("0.9.0"));
    }
}
