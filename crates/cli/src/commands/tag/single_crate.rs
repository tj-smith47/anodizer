use super::*;

/// Bring one configured crate's manifests to `new_version`: rewrite its own
/// `[package].version`, propagate that version into sibling `path + version`
/// dep floors, and heal every remaining stale floor in the Cargo workspace
/// that owns it.
///
/// `crate_path` is the config-declared, repo-root-relative crate directory;
/// it is resolved against `workspace_root` so the manifest IO hits the tree
/// git operates on even when `tag` runs from a subdirectory. Returns the
/// crate's package name (`None` when its manifest is unreadable) and the
/// manifests the dep edits touched, for staging into the bump commit.
pub(crate) fn sync_single_crate_manifests(
    workspace_root: &Path,
    crate_path: &str,
    new_version: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<(Option<String>, Vec<String>)> {
    let abs_crate_dir = workspace_root.join(crate_path);
    anodizer_stage_build::version_sync::sync_version(
        workspace_root,
        crate_path,
        new_version,
        dry_run,
        log,
    )?;

    // An unreadable or malformed manifest is an error: the crate's own version
    // was just rewritten, so skipping the sibling dep-spec update would leave
    // siblings pinned to the version before it.
    let crate_name = crate::commands::bump::cargo_edit::parse_member_manifest(
        &abs_crate_dir.join("Cargo.toml"),
    )?
    .map(|m| m.name);

    // Update dependency version specs in other crates that belong to the SAME
    // Cargo workspace as the bumped crate. Scoping to the owning workspace
    // prevents this bump from rewriting a path-dep pin in an independent
    // release group on a different cadence.
    let mut dep_modified: Vec<String> = match crate_name {
        Some(ref name) => anodizer_stage_build::version_sync::sync_workspace_deps(
            workspace_root,
            &abs_crate_dir,
            name,
            new_version,
            dry_run,
            log,
        )?
        .iter()
        .map(|p| anodizer_core::path_util::display_under_root(workspace_root, Path::new(p)))
        .collect(),
        None => Vec::new(),
    };

    // Raise every remaining stale internal floor in the owning workspace,
    // including floors on siblings this run does not release: a bump commit
    // stranded off the default branch otherwise leaves the branch resolving an
    // old sibling at the next publish.
    let heal_scope = anodizer_stage_build::version_sync::cargo_workspace_root_for(
        workspace_root,
        &abs_crate_dir,
    );
    // Under `--dry-run` the manifest still holds the old version, so the map is
    // what makes the preview resolve this crate to the version the real run
    // writes; the floors `sync_workspace_deps` owns are left to it in both modes.
    let pending: std::collections::BTreeMap<String, String> = crate_name
        .iter()
        .map(|name| (name.clone(), new_version.to_string()))
        .collect();
    for healed in heal_dep_floors(
        &heal_scope,
        Propagated::TopLevelSections(&pending),
        dry_run,
        log,
    )? {
        let path = anodizer_core::path_util::display_under_root(workspace_root, &healed);
        if !dep_modified.contains(&path) {
            dep_modified.push(path);
        }
    }

    Ok((crate_name, dep_modified))
}
