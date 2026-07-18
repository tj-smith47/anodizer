use super::*;

/// Validate workspace names (non-empty, unique) plus per-workspace crate
/// names, tag templates, and `depends_on` references (resolved against the
/// flattened crate set so cross-workspace refs are accepted).
pub(super) fn check_workspaces(
    config: &Config,
    all_crate_names: &HashSet<&str>,
    errors: &mut Vec<String>,
) {
    let Some(ref workspaces) = config.workspaces else {
        return;
    };

    let mut seen_names: HashSet<&str> = HashSet::new();
    for (i, ws) in workspaces.iter().enumerate() {
        if ws.name.trim().is_empty() {
            errors.push(format!("workspace at index {}: name must not be empty", i));
        } else if !seen_names.insert(ws.name.as_str()) {
            errors.push(format!("duplicate workspace name '{}'", ws.name));
        }
    }

    for ws in workspaces {
        let mut ws_crate_names: HashSet<&str> = HashSet::new();
        for (i, c) in ws.crates.iter().enumerate() {
            if c.name.trim().is_empty() {
                errors.push(format!(
                    "workspace '{}': crate at index {}: name must not be empty",
                    ws.name, i
                ));
            } else if !ws_crate_names.insert(c.name.as_str()) {
                errors.push(format!(
                    "workspace '{}': duplicate crate name '{}'",
                    ws.name, c.name
                ));
            }
        }
        for c in &ws.crates {
            validate_tag_template(
                c.tag_template.as_deref().unwrap_or(""),
                &format!("workspace '{}': crate '{}'", ws.name, c.name),
                errors,
            );
        }
        for c in &ws.crates {
            if let Some(deps) = &c.depends_on {
                for dep in deps {
                    if !all_crate_names.contains(dep.as_str()) {
                        errors.push(format!(
                            "workspace '{}': crate '{}': depends_on '{}' does not exist",
                            ws.name, c.name, dep
                        ));
                    }
                }
            }
        }
    }
}

/// Top-level crate names must be non-empty.
pub(super) fn check_top_level_crate_names(config: &Config, errors: &mut Vec<String>) {
    // Raw walk (not `crate_universe()`): index-based messages need the
    // top-level declaration order, and the walker's dedup would hide a
    // duplicate entry from validation. Workspace crate names get the same
    // check from `check_workspaces`'s own loop.
    for (i, c) in config.crates.iter().enumerate() {
        if c.name.trim().is_empty() {
            errors.push(format!("crate at index {}: name must not be empty", i));
        }
    }
}

/// Top-level `depends_on` references must resolve against the flattened
/// crate set so a top-level crate can depend on a crate that lives in a
/// workspace.
pub(super) fn check_top_level_depends_on(
    config: &Config,
    all_crate_names: &HashSet<&str>,
    errors: &mut Vec<String>,
) {
    // Raw walk (not `crate_universe()`): every entry as written must be
    // validated, including one the walker's dedup would shadow. Workspace
    // crates' `depends_on` gets the same check from `check_workspaces`.
    for c in &config.crates {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if !all_crate_names.contains(dep.as_str()) {
                    errors.push(format!(
                        "crate '{}': depends_on '{}' does not exist",
                        c.name, dep
                    ));
                }
            }
        }
    }
}

/// DFS-based cycle detection across the whole crate universe. The release
/// engine topo-sorts the flattened set, so a cycle through a workspace crate
/// breaks a release exactly like a top-level one and must be flagged here.
pub(super) fn check_cycles(config: &Config, errors: &mut Vec<String>) {
    let universe: Vec<CrateConfig> = config.crate_universe().into_iter().cloned().collect();
    if let Some(cycle) = find_cycle(&universe) {
        errors.push(format!("depends_on cycle detected: {}", cycle.join(" → ")));
    }
}

/// Top-level `tag_template` must contain `{{ .Version }}` or `{{ Version }}`
/// (Tera-native).
pub(super) fn check_top_level_tag_templates(config: &Config, errors: &mut Vec<String>) {
    // Raw walk (not `crate_universe()`): every entry as written must be
    // validated, including one the walker's dedup would shadow. Workspace
    // crates' tag templates get the same check from `check_workspaces`.
    for c in &config.crates {
        validate_tag_template(
            c.tag_template.as_deref().unwrap_or(""),
            &format!("crate '{}'", c.name),
            errors,
        );
    }
}

/// Each build's `copy_from` must reference a binary defined in the same
/// crate's builds. The effective binary name falls back to the crate name
/// when the per-build `binary` field is omitted (e.g. when defaults supply a
/// template without `binary:`).
pub(super) fn check_copy_from(config: &Config, errors: &mut Vec<String>) {
    for c in config.crate_universe() {
        if let Some(builds) = &c.builds {
            let effective: Vec<&str> = builds
                .iter()
                .map(|b| b.binary.as_deref().unwrap_or(c.name.as_str()))
                .collect();
            let binaries: HashSet<&str> = effective.iter().copied().collect();
            for (idx, build) in builds.iter().enumerate() {
                let bin = effective[idx];
                if let Some(copy_from) = &build.copy_from
                    && !binaries.contains(copy_from.as_str())
                {
                    errors.push(format!(
                        "crate '{}': build binary '{}' has copy_from '{}' which is not a binary in this crate",
                        c.name, bin, copy_from
                    ));
                }
            }
        }
    }
}

/// Each non-empty crate `path` must point to an existing directory.
pub(super) fn check_crate_paths(config: &Config, errors: &mut Vec<String>) {
    for c in config.crate_universe() {
        if !c.path.is_empty() {
            let p = std::path::Path::new(&c.path);
            if !p.exists() {
                errors.push(format!(
                    "crate '{}': path '{}' does not exist",
                    c.name, c.path
                ));
            }
        }
    }
}

/// Whether `c` actually publishes to crates.io: `publish.cargo` presence
/// opts a crate in; `cargo: { skip: true }` opts back out. A crate with no
/// active cargo publisher never runs `cargo publish`, so a missing
/// intra-workspace dependency can never break its publish — mirrored by
/// `crate_has_active_cargo_publisher`'s use as the membership-check gate.
pub(super) fn crate_has_active_cargo_publisher(c: &CrateConfig) -> bool {
    c.publish
        .as_ref()
        .and_then(|p| p.cargo.as_ref())
        .is_some_and(|cargo| !cargo.skip.as_ref().is_some_and(|s| s.as_bool()))
}

/// Walk up from `start_dir` to find the nearest ancestor whose `Cargo.toml`
/// declares a `[workspace]` table — the same directory-climbing resolution
/// `cargo` itself uses to locate a member's workspace root. Falls back to
/// `start_dir` when no ancestor `Cargo.toml` declares one (e.g. the crate is
/// a standalone package, or `start_dir` doesn't exist).
///
/// Deliberately unbounded, mirroring cargo: cargo's own workspace-root
/// resolution climbs to the filesystem root with no depth limit, and a
/// nested workspace (a sub-crate's ancestor Cargo.toml declaring its OWN
/// `[workspace]` closer than the outer one) is a real, supported layout —
/// capping the climb here would find the wrong root and silently diverge
/// from what `cargo publish` itself would resolve. Every `read_to_string` /
/// `toml::from_str` failure along the way is `.ok()`-swallowed rather than
/// surfaced: this function only READS candidate `Cargo.toml`s to test for a
/// `[workspace]` table, so a missing or malformed file at any ancestor is
/// just "not a workspace root here" — never a mutation, never unsafe to
/// ignore.
pub(super) fn find_cargo_workspace_root(start_dir: &Path) -> PathBuf {
    let mut dir = start_dir;
    loop {
        let content = std::fs::read_to_string(dir.join("Cargo.toml")).ok();
        let parsed = content.and_then(|s| toml::from_str::<toml::Value>(&s).ok());
        let has_workspace_table = parsed.is_some_and(|doc| {
            doc.get("workspace")
                .and_then(toml::Value::as_table)
                .is_some()
        });
        if has_workspace_table {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start_dir.to_path_buf(),
        }
    }
}

/// FAILS when a real on-disk Cargo workspace member is an intra-workspace
/// dependency (per its `Cargo.toml [dependencies]`) of a crate in `crates:`,
/// but is itself absent from `crates:` — the exact v0.19.0 class of failure
/// (`anodizer-stage-install-script` was a CLI dependency on disk but missing
/// from `crates:`, so cargo failed the CLI's publish upload with "no
/// matching package named ... found"). Also FAILS when the dependency IS
/// present in `crates:` but has no active cargo publisher itself (skipped or
/// never configured) — cargo will still fail the dependent's publish because
/// the dependency is never uploaded to the registry.
///
/// Gated on the dependent crate actually having an active cargo publisher:
/// a crate that never runs `cargo publish` can't be broken by a missing
/// workspace dependency, so it must not raise a false-positive error.
///
/// Each crate's own on-disk Cargo workspace root is resolved independently
/// (directory-climbing from the crate's path, cached per root) rather than
/// assumed to be `base_dir` — a `workspaces:` (multi-cargo-workspace) config
/// commonly spans several distinct physical Cargo workspaces below the
/// anodizer config root, and reading only `base_dir`'s `Cargo.toml` would
/// silently derive an empty member set for every crate that lives in a
/// sub-workspace.
pub(super) fn check_workspace_membership(
    config: &Config,
    base_dir: &Path,
    all_crate_names: &HashSet<&str>,
    errors: &mut Vec<String>,
) {
    let mut member_cache: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    for c in config.crate_universe() {
        if c.path.is_empty() || !crate_has_active_cargo_publisher(c) {
            continue;
        }
        let crate_dir = base_dir.join(&c.path);
        let root = find_cargo_workspace_root(&crate_dir);
        let member_names = member_cache
            .entry(root.clone())
            .or_insert_with(|| anodizer_core::config::discover_cargo_workspace_member_names(&root));
        if member_names.is_empty() {
            continue;
        }
        let deps =
            anodizer_core::config::derive_depends_on_from_cargo_toml(&crate_dir, member_names);
        for dep in deps {
            if !all_crate_names.contains(dep.as_str()) {
                errors.push(format!(
                    "crate '{dep}' is a workspace member and an intra-workspace \
                     dependency of published crate '{}', but is absent from \
                     `crates:` (cargo will fail publishing '{}')",
                    c.name, c.name
                ));
            } else if let Some(dep_cfg) = config.find_crate(dep.as_str())
                && !crate_has_active_cargo_publisher(dep_cfg)
            {
                errors.push(format!(
                    "crate '{dep}' is an intra-workspace dependency of published \
                     crate '{}' but has no active cargo publisher (skipped or \
                     never configured for crates.io) — cargo will fail publishing \
                     '{}' because '{dep}' is never uploaded to the registry",
                    c.name, c.name
                ));
            }
        }
    }
}
