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

    let tag_prefix = git::per_crate_tag_prefix(
        &crate_cfg.name,
        crate_cfg.tag_template.as_deref().unwrap_or(""),
    );
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
