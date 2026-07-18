use super::*;

/// Resolve the crate selection (`--all` + change detection, `--all --force`,
/// explicit `--crate` list, or tags-at-HEAD default) and topologically sort it.
pub(crate) fn resolve_selected_crates(
    opts: &ReleaseOpts,
    all_known_crates: &[CrateConfig],
    config: &Config,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let selected = if opts.all {
        if opts.force {
            all_known_crates.iter().map(|c| c.name.clone()).collect()
        } else {
            // Resolve crate + workspace-file pathspecs against the discovered
            // Cargo workspace root, not the process CWD, so change detection is
            // identical whether `release` is invoked from the root or a
            // subdirectory. This mirrors the unification `tag`/`changelog`/`bump`
            // already share via `discover_workspace_root`. From a subdir the old
            // cwd anchor mis-resolved every pathspec: per-crate paths pointed at
            // `<subdir>/crates/<x>` (no match → under-detect, masked by the
            // empty-means-all collapse) and the workspace-level `Cargo.toml`
            // pathspec pointed at the subdir's own manifest (a per-crate manifest
            // edit then false-promoted the entire workspace).
            let workspace_root =
                crate::commands::helpers::discover_workspace_root(opts.config_override.as_deref())?;
            detect_changed_crates(
                &workspace_root,
                all_known_crates,
                config.git.as_ref(),
                config.monorepo_tag_prefix(),
                log,
            )?
        }
    } else if !opts.crate_names.is_empty() {
        opts.crate_names.clone()
    } else {
        // Default: read tags pointing at HEAD and map each to a crate.
        map_head_tags_to_crates(all_known_crates, log)?
    };
    Ok(topo_sort_selected(all_known_crates, &selected))
}

/// Read tags pointing at HEAD and resolve each to a crate name via
/// per-crate `tag_template` prefix matching.
///
/// Tags that don't match any configured crate are silently ignored — this
/// allows foreign tags (e.g. a nightly build tag) to coexist without
/// aborting the release pipeline.
///
/// Returns an empty vec when HEAD has no tags; the caller treats that as a
/// no-op.
pub(crate) fn map_head_tags_to_crates(
    all_known_crates: &[CrateConfig],
    log: &StageLogger,
) -> Result<Vec<String>> {
    let head_tags = git::get_tags_at_head().with_context(|| "failed to read tags at HEAD")?;
    if head_tags.is_empty() {
        log.verbose("no tags at HEAD — release no-op");
        return Ok(Vec::new());
    }
    log.verbose(&format!("tags at HEAD = {}", head_tags.join(", ")));
    Ok(select_crates_for_tags(&head_tags, all_known_crates, log))
}

/// Map a concrete list of tags to the set of crates they select, in
/// first-seen order, deduped by name.
///
/// Split out of [`map_head_tags_to_crates`] so the selection logic — the
/// lockstep tie-tier expansion that the whole `--publish-only` fix turns on
/// — is unit-testable without a git fixture. The only thing the wrapper adds
/// is reading `git::get_tags_at_head()`; everything that decides WHICH crates
/// a tag selects lives here.
pub(crate) fn select_crates_for_tags(
    head_tags: &[String],
    all_known_crates: &[CrateConfig],
    log: &StageLogger,
) -> Vec<String> {
    let mut selected: Vec<String> = Vec::new();
    for tag in head_tags {
        let matches = resolve_tag_to_crates(tag, all_known_crates);
        if matches.is_empty() {
            log.verbose(&format!(
                "skipped tag '{}' — matches no configured crate",
                tag
            ));
            continue;
        }
        for c in matches {
            if !selected.contains(&c.name) {
                selected.push(c.name.clone());
                log.verbose(&format!("tag '{}' → crate '{}'", tag, c.name));
            }
        }
    }
    selected
}

/// Resolve a single tag to EVERY crate sharing the longest-matching
/// `tag_template` prefix tier.
///
/// In a lockstep workspace every crate carries the SAME `tag_template`
/// (e.g. `v{{ Version }}`), so a single pushed tag (`v1.0.0`) legitimately
/// means "release ALL crates", not just the first-declared one. The
/// singular resolver returned only the first match, which silently dropped
/// every sibling crate — including a binary/artifact-publishing crate whose
/// `publish:` block (scoop / chocolatey / winget / aur / nix) then never ran
/// because the publish stage's effective-crate set was scoped to the wrong
/// (first-declared, publisher-less) crate. Returning the whole tie-tier keeps
/// the bin crate in the selection so its publishers fire.
///
/// "Tier" = the set of crates whose extracted prefix has the maximum length
/// among all crates matching `tag`. A more specific crate (`core-v`, length 6)
/// still wins exclusively over a shorter sibling (`v`, length 1) — the
/// per-crate INDEPENDENT-tag workspace mode is unchanged, because distinct
/// tags at HEAD each resolve to their own single longest-prefix crate.
/// Declaration order within the winning tier is preserved.
pub(crate) fn resolve_tag_to_crates<'a>(
    tag: &str,
    crates: &'a [CrateConfig],
) -> Vec<&'a CrateConfig> {
    let mut best_len: Option<usize> = None;
    let mut matched: Vec<(&CrateConfig, usize)> = Vec::new();
    for c in crates {
        if let Some(prefix) = git::extract_tag_prefix(c.tag_template.as_deref().unwrap_or(""))
            && tag.starts_with(&prefix)
        {
            let remainder = &tag[prefix.len()..];
            let is_version = remainder
                .split('.')
                .next()
                .is_some_and(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit()));
            if is_version {
                let len = prefix.len();
                best_len = Some(best_len.map_or(len, |b| b.max(len)));
                matched.push((c, len));
            }
        }
    }
    let Some(best) = best_len else {
        return Vec::new();
    };
    matched
        .into_iter()
        .filter(|(_, len)| *len == best)
        .map(|(c, _)| c)
        .collect()
}

/// Stages snapshot mode auto-skips: every stage that performs an external
/// upload with no snapshot-aware internal gate. Deliberately NARROWER than
/// [`anodizer_core::stages::UPSTREAM_STAGES`]: the remaining upstream
/// stages (`release`, `docker`, `docker-sign`, `verify-release`) gate their
/// upstream side effects on snapshot mode internally (the release stage
/// short-circuits, docker's push flag is disabled) and keep local work
/// worth running in a snapshot. Pinned as a subset of `UPSTREAM_STAGES` by
/// test so a future upstream stage must be classified here explicitly.
pub(crate) const SNAPSHOT_AUTO_SKIP: &[&str] =
    &["publish", "snapcraft-publish", "blob", "announce"];

/// Merge CLI / workspace / snapshot-implied skip stages into one list.
/// Snapshot mode auto-skips [`SNAPSHOT_AUTO_SKIP`]. Skipping `publish`
/// implies skipping `announce`.
pub(crate) fn compute_skip_stages(
    mut skip_stages: Vec<String>,
    workspace_skip: &[String],
    snapshot: bool,
) -> Vec<String> {
    helpers::merge_skip_stages(&mut skip_stages, workspace_skip);
    if snapshot {
        helpers::merge_skip_stages(&mut skip_stages, SNAPSHOT_AUTO_SKIP);
    }
    if skip_stages.contains(&"publish".to_string())
        && !skip_stages.contains(&"announce".to_string())
    {
        skip_stages.push("announce".to_string());
    }
    skip_stages
}
/// Detect which crates have changes since their last tag.
pub(crate) fn detect_changed_crates_pub(
    workspace_root: &Path,
    crates: &[CrateConfig],
    git_config: Option<&anodizer_core::config::GitConfig>,
    monorepo_prefix: Option<&str>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    detect_changed_crates(workspace_root, crates, git_config, monorepo_prefix, log)
}

pub(crate) fn detect_changed_crates(
    workspace_root: &Path,
    crates: &[CrateConfig],
    git_config: Option<&anodizer_core::config::GitConfig>,
    monorepo_prefix: Option<&str>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    // Log when ignore_tags/ignore_tag_prefixes contain template expressions
    // but template_vars are not yet available (we pass None below).
    if let Some(gc) = git_config {
        let has_templates = gc
            .ignore_tags
            .as_ref()
            .is_some_and(|tags| tags.iter().any(|t| t.contains("{{")))
            || gc
                .ignore_tag_prefixes
                .as_ref()
                .is_some_and(|pfx| pfx.iter().any(|p| p.contains("{{")));
        if has_templates {
            log.debug(
                "ignore_tags/ignore_tag_prefixes templates not rendered during \
                 change detection (template vars not yet available)",
            );
        }
    }

    let mut changed = vec![];
    let mut oldest_tag: Option<String> = None;

    for c in crates {
        let latest_tag = git::find_latest_tag_matching_with_prefix(
            c.resolved_tag_template(),
            git_config,
            None,
            monorepo_prefix,
        )?;
        match &latest_tag {
            None => {
                // No tag at all → always include
                changed.push(c.name.clone());
            }
            Some(tag) => {
                if git::has_changes_since_in(workspace_root, tag, &c.path)? {
                    changed.push(c.name.clone());
                }
                // Track the earliest tag for workspace-level check
                if let Ok(sv) = git::parse_semver_tag(tag) {
                    let is_older = oldest_tag
                        .as_ref()
                        .and_then(|t| git::parse_semver_tag(t).ok())
                        .is_none_or(|osv| sv < osv);
                    if is_older {
                        oldest_tag = Some(tag.clone());
                    }
                }
            }
        }
    }

    // Propagate changes transitively via depends_on: if crate B depends on
    // changed crate A, include B too. Use a fixed-point loop.
    changed = propagate_dependents(crates, changed);

    // Check workspace-level files against the oldest tag
    if let Some(ref tag) = oldest_tag {
        let ws_changed = check_workspace_files_changed(workspace_root, tag)?;
        if ws_changed {
            // Include all crates
            return Ok(crates.iter().map(|c| c.name.clone()).collect());
        }
    }

    Ok(changed)
}

/// Transitively propagate changed crates via `depends_on`.
///
/// If crate B depends on changed crate A, B is also included. Repeats until
/// the set stabilises (fixed-point loop).
pub(crate) fn propagate_dependents(crates: &[CrateConfig], changed: Vec<String>) -> Vec<String> {
    use std::collections::HashSet;

    let changed_set: HashSet<String> = changed.iter().cloned().collect();
    let mut result_set = changed_set;

    loop {
        let mut added = false;
        for c in crates {
            if result_set.contains(&c.name) {
                continue;
            }
            if let Some(deps) = &c.depends_on
                && deps.iter().any(|dep| result_set.contains(dep))
            {
                result_set.insert(c.name.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    // Preserve original order from `changed`, then append newly added crates
    let mut propagated: Vec<String> = Vec::new();
    for name in &changed {
        if result_set.contains(name) {
            propagated.push(name.clone());
        }
    }
    for c in crates {
        if result_set.contains(&c.name) && !changed.contains(&c.name) {
            propagated.push(c.name.clone());
        }
    }
    propagated
}

/// Check if workspace-level files (Cargo.toml, Cargo.lock) changed since tag.
///
/// Pathspecs resolve against `workspace_root` (the discovered Cargo root), not
/// the process CWD, so a `release --all` from a subdirectory inspects the root
/// manifests rather than the subdir's own `Cargo.toml`/`Cargo.lock`.
pub(crate) fn check_workspace_files_changed(workspace_root: &Path, tag: &str) -> Result<bool> {
    anodizer_core::git::paths_changed_since_tag_in(
        workspace_root,
        tag,
        &["Cargo.toml", "Cargo.lock"],
    )
}

/// Topologically sort the selected crates respecting depends_on order.
pub(crate) fn topo_sort_selected(all_crates: &[CrateConfig], selected: &[String]) -> Vec<String> {
    let selected_set: std::collections::HashSet<&str> =
        selected.iter().map(|s| s.as_str()).collect();

    let items: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected_set.contains(c.name.as_str()))
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    anodizer_core::util::topological_sort(&items)
}
