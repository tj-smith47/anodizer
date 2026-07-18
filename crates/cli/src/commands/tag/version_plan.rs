use super::*;

/// Rewrite the old version to the new version in every enrolled `version_files`
/// entry, log the per-file outcome, and return the repo-relative paths that
/// actually changed (so the caller can stage them into the bump commit).
///
/// Enrolled paths are repo-root-relative; each is resolved against `root` (the
/// discovered workspace root) for the read/write so the rewrite hits the same
/// files git operates on even when `tag` is invoked from a subdirectory. The
/// logged and returned paths stay repo-relative so staging via
/// [`git::stage_and_commit_in`] (rooted at the same `root`) matches.
///
/// A file with zero matches is reported via `warn` but is not an error: a stale
/// enrollment should surface loudly without aborting the tag. When `dry_run` is
/// set, counts are logged but no file is written and no path is returned for
/// staging. A no-op (`old == new`) returns immediately.
pub(crate) fn rewrite_and_stage_version_files(
    root: &Path,
    files: &[String],
    old: &str,
    new: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    if files.is_empty() || old == new {
        return Ok(Vec::new());
    }
    let resolved: Vec<String> = files
        .iter()
        .map(|f| root.join(f).to_string_lossy().into_owned())
        .collect();
    let outcomes =
        anodizer_core::version_files::rewrite_version_in_files(&resolved, old, new, dry_run)?;
    let mut changed = Vec::new();
    for (outcome, rel) in outcomes.iter().zip(files.iter()) {
        if outcome.replacements > 0 {
            log.status(&format!(
                "{}rewrote {} occurrence(s) of {} → {} in {}",
                if dry_run { "(dry-run) " } else { "" },
                outcome.replacements,
                old,
                new,
                rel
            ));
            if !dry_run {
                changed.push(rel.clone());
            }
        } else {
            log.warn(&format!(
                "enrolled version_files entry {} did not contain version {} (nothing rewritten)",
                rel, old
            ));
        }
    }
    Ok(changed)
}
/// Build the deduped, conflict-checked set of `version_files` rewrites across
/// every per-crate group, in first-seen order.
///
/// A shared enrolled path is a conflict whenever two crates enrolling it carry
/// NON-IDENTICAL `(old, new)` pairs — a file cannot simultaneously rewrite
/// `0.1.0 → 0.2.0` and `0.1.5 → 0.2.0` (the second crate's occurrences would be
/// left stale), nor hold two different new versions at once. Identical pairs
/// dedupe to a single rewrite (lockstep crates share one pair, so they never
/// conflict). On conflict this `bail!`s naming the file and both crates/pairs.
///
/// Runs identically for dry-run and real tagging so the preview matches the
/// outcome — the validated plan is computed once, then either previewed or
/// applied by the caller.
pub(crate) fn plan_version_files_rewrites(
    tag_results: &[GroupTagResult],
) -> Result<Vec<VersionFileRewrite>> {
    use std::collections::HashMap;
    // file → (old, new, owning-crate) of the first group that enrolled it.
    let mut seen: HashMap<String, (String, String, String)> = HashMap::new();
    let mut plan: Vec<VersionFileRewrite> = Vec::new();

    for group_result in tag_results {
        let Some(ref old) = group_result.old_version else {
            continue;
        };
        let owner = group_result
            .crate_names
            .first()
            .cloned()
            .unwrap_or_else(|| "?".to_string());
        for ((_, new_version), files) in group_result
            .version_updates
            .iter()
            .zip(group_result.crate_version_files.iter())
        {
            for file in files {
                match seen.get(file) {
                    Some((existing_old, existing_new, existing_crate))
                        if existing_old != old || existing_new != new_version =>
                    {
                        bail!(
                            "version_files conflict: {} is enrolled by crates with different \
                             version bumps ({} {} → {} vs {} {} → {}); a file cannot hold two \
                             versions in one tag run",
                            file,
                            existing_crate,
                            existing_old,
                            existing_new,
                            owner,
                            old,
                            new_version,
                        );
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(
                            file.clone(),
                            (old.clone(), new_version.clone(), owner.clone()),
                        );
                        plan.push(VersionFileRewrite {
                            file: file.clone(),
                            old: old.clone(),
                            new: new_version.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(plan)
}

/// Build one [`ChangelogTarget`] per bumped crate across all groups.
///
/// Each crate renders from ITS group's previous tag (`prev_tag`) to ITS new
/// version, so independently-versioned crates each get a section keyed to their
/// own bump. `crate_dir` is resolved to an absolute path under `workspace_root`
/// so the changelog engine reads/writes the correct `CHANGELOG.md`.
pub(crate) fn plan_changelog_targets(
    workspace_root: &Path,
    tag_results: &[GroupTagResult],
) -> Vec<ChangelogTarget> {
    let mut targets = Vec::new();
    for group_result in tag_results {
        for ((crate_name, (crate_path, new_version)), (full_tag, _msg)) in group_result
            .crate_names
            .iter()
            .zip(group_result.version_updates.iter())
            .zip(group_result.new_tags.iter())
        {
            targets.push(ChangelogTarget {
                crate_name: crate_name.clone(),
                crate_dir: workspace_root.join(crate_path),
                from_tag: group_result.prev_tag.clone(),
                to_version: new_version.clone(),
                full_tag: full_tag.clone(),
            });
        }
    }
    targets
}

/// Collapse `targets` in place to ONE flat whole-workspace aggregate when
/// `collapse` is set (the caller has resolved a `FlatAggregate` shape routed to
/// one shared root). Returns `true` when collapsed (the caller then sets the
/// routing's `single_track`), `false` otherwise (`targets` left untouched).
///
/// The flat-aggregate DECISION lives in [`detect_repo_shape`] (via
/// [`prefix_groups`]); this helper only applies it, so the prefix-equality
/// comparison is not re-derived here.
///
/// The aggregate spans the workspace (`crate_dir = workspace_root`), keyed by
/// `project_name`, with the shared `from_tag` / `full_tag` every member already
/// carries (identical across a lockstep set).
pub(crate) fn collapse_targets_to_flat_aggregate(
    targets: &mut Vec<ChangelogTarget>,
    workspace_root: &Path,
    config: Option<&anodizer_core::config::Config>,
    collapse: bool,
) -> bool {
    if !collapse || targets.len() <= 1 {
        return false;
    }
    let Some(config) = config else {
        return false;
    };
    let project_name = config.project_name.clone();
    // Every member shares one tag in a lockstep set; take the first's range
    // bounds for the whole-release aggregate.
    let first = match targets.first() {
        Some(t) => t,
        None => return false,
    };
    let aggregate = ChangelogTarget {
        crate_name: project_name,
        crate_dir: workspace_root.to_path_buf(),
        from_tag: first.from_tag.clone(),
        to_version: first.to_version.clone(),
        full_tag: first.full_tag.clone(),
    };
    *targets = vec![aggregate];
    true
}
