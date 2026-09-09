use super::*;

/// When `--crate` is specified, look up the crate in top-level crates and
/// workspace crates.  Returns the tag prefix (from `tag_template`) and the
/// crate's `path` so change detection can be scoped to that directory.
///
/// `None` only for an UNKNOWN crate name (the caller validates and errors
/// first). A known crate whose template has no extractable prefix resolves
/// to the canonical `<name>-v` fallback family via
/// [`git::per_crate_tag_prefix`] — the same family the per-crate engine and
/// the changelog's crate selection scan, so `--crate` never silently
/// switches a crate to the repo-level `v` namespace.
///
/// Takes the command's single shared config load rather than re-loading:
/// every `load_config` re-emits the load-time legacy-alias warnings, so a
/// second load doubled them on the `--crate` path.
pub(crate) fn load_crate_tag_info(
    config: &anodizer_core::config::Config,
    crate_name: &str,
) -> Option<CrateTagInfo> {
    let crate_cfg = config.find_crate(crate_name)?;

    let tag_prefix = git::per_crate_tag_prefix(&crate_cfg.name, &crate_cfg.tag_family_template());
    let version_sync = crate_cfg
        .version_sync
        .as_ref()
        .and_then(|vs| vs.enabled)
        .unwrap_or(false);
    let version_files = resolve_version_files(Some(crate_cfg), Some(config));
    Some(CrateTagInfo {
        tag_prefix,
        path: crate_cfg.path.clone(),
        version_sync,
        version_files,
    })
}

/// Find the previous tag for version derivation.
///
/// When `remote_tags` is `Some` (an `origin`-style remote exists and its tag
/// list was fetched), local candidates absent from the remote are dropped:
/// a tag deleted on the remote (the documented re-cut recipe) must not count
/// as "previous" just because a clone still holds it. Remote-only tags are
/// not added — commit-range scans against them could not resolve locally.
pub(crate) fn find_previous_tag(
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    remote_tags: Option<&std::collections::HashSet<String>>,
) -> Result<Option<String>> {
    let mut tags = match cfg.tag_context.as_str() {
        "branch" => git::get_branch_semver_tags(&cfg.tag_prefix, git_config, None)?,
        _ => git::get_all_semver_tags(&cfg.tag_prefix, git_config, None)?,
    };
    if let Some(remote) = remote_tags {
        tags.retain(|t| remote.contains(t));
    }

    let tag_sort = git_config
        .and_then(|gc| gc.tag_sort.as_deref())
        .unwrap_or("-version:refname");
    if tag_sort == "smartsemver" && !cfg.prerelease {
        // When targeting a non-prerelease version, skip prerelease candidates
        // so the changelog base points at the previous stable release rather
        // than an intervening beta or RC.
        for tag in tags {
            if let Ok(sv) = git::parse_semver_tag(&tag)
                && !sv.is_prerelease()
            {
                return Ok(Some(tag));
            }
        }
        return Ok(None);
    }

    Ok(tags.into_iter().next())
}

pub(crate) fn branch_matches(branch: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        // Try exact match first
        if branch == pattern {
            return true;
        }
        // Try regex match (anchored to prevent partial matches)
        if let Ok(re) = Regex::new(&format!("^{}$", pattern))
            && re.is_match(branch)
        {
            return true;
        }
    }
    false
}

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
    let abs_crate_dir = workspace_root
        .join(crate_path)
        .to_string_lossy()
        .into_owned();
    anodizer_stage_build::version_sync::sync_version(
        workspace_root,
        crate_path,
        new_version,
        dry_run,
        log,
    )?;

    let crate_name = std::fs::read_to_string(Path::new(&abs_crate_dir).join("Cargo.toml"))
        .ok()
        .and_then(|content| content.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        });

    // Update dependency version specs in other crates that belong to the SAME
    // Cargo workspace as the bumped crate. Scoping to the owning workspace
    // prevents this bump from rewriting a path-dep pin in an independent
    // release group on a different cadence.
    let mut dep_modified = match crate_name {
        Some(ref name) => anodizer_stage_build::version_sync::sync_workspace_deps(
            &workspace_root.to_string_lossy(),
            &abs_crate_dir,
            name,
            new_version,
            dry_run,
            log,
        )?,
        None => Vec::new(),
    };

    // Raise every remaining stale internal floor in the owning workspace,
    // including floors on siblings this run does not release: a bump commit
    // stranded off the default branch otherwise leaves the branch resolving an
    // old sibling at the next publish.
    let heal_scope = anodizer_stage_build::version_sync::cargo_workspace_root_for(
        workspace_root,
        Path::new(&abs_crate_dir),
    );
    // On the real path `sync_version` has already written the new version to
    // the manifest, so `pending` only restates it; under `--dry-run` nothing is
    // on disk to restate and a floor on this crate belongs to the propagation
    // above, not to the sweep.
    let pending: std::collections::BTreeMap<String, String> = crate_name
        .iter()
        .filter(|_| !dry_run)
        .map(|name| (name.clone(), new_version.to_string()))
        .collect();
    for healed in heal_dep_floors(&heal_scope, &pending, dry_run, log)? {
        let path = healed
            .strip_prefix(workspace_root)
            .unwrap_or(healed.as_path())
            .to_string_lossy()
            .into_owned();
        if !dep_modified.contains(&path) {
            dep_modified.push(path);
        }
    }

    Ok((crate_name, dep_modified))
}
