use super::*;

/// Apply every planned `version_files` rewrite, log the per-entry outcome, and
/// return the repo-relative paths that actually changed (so the caller can
/// stage them into the bump commit).
///
/// Enrolled paths are repo-root-relative; each is resolved against `root` (the
/// discovered workspace root) for the read/write so the rewrite hits the same
/// files git operates on even when `tag` is invoked from a subdirectory. The
/// logged and returned paths stay repo-relative so staging via
/// [`git::stage_and_commit_in`] (rooted at the same `root`) matches.
///
/// A BARE entry with zero matches is reported via `warn` but is not an error: a
/// stale enrollment should surface loudly without aborting the tag. An ANCHORED
/// entry that selects no region IS an error — it states a precise intent, and
/// the engine raises it before writing any file, so nothing is half-applied.
/// When `dry_run` is set, counts are logged but no file is written and no path
/// is returned for staging. A no-op entry (`old == new`) is dropped.
pub(crate) fn rewrite_and_stage_version_files(
    root: &Path,
    plan: &[VersionFileRewrite],
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let applicable: Vec<&VersionFileRewrite> = plan.iter().filter(|r| r.old != r.new).collect();
    if applicable.is_empty() {
        return Ok(Vec::new());
    }
    let rewrites: Vec<anodizer_core::version_files::FileRewrite> = applicable
        .iter()
        .map(|r| anodizer_core::version_files::FileRewrite {
            path: root.join(&r.file).to_string_lossy().into_owned(),
            anchor: r.anchor.clone(),
            old: r.old.clone(),
            new: r.new.clone(),
            owner: r.owner.clone(),
        })
        .collect();
    let outcomes = anodizer_core::version_files::rewrite_version_in_files(&rewrites, dry_run)?;
    let mut changed = Vec::new();
    for (outcome, rewrite) in outcomes.iter().zip(applicable.iter()) {
        let anchor_suffix = match &rewrite.anchor {
            Some(anchor) => {
                log.verbose(&format!(
                    "version_files: {} anchor {} matched {} region(s)",
                    rewrite.file,
                    anchor,
                    outcome.matched_regions.unwrap_or(0)
                ));
                format!(" (match {anchor})")
            }
            None => String::new(),
        };
        if outcome.replacements > 0 {
            log.status(&format!(
                "{}rewrote {} occurrence(s) of {} → {} in {}{}",
                if dry_run { "(dry-run) " } else { "" },
                outcome.replacements,
                rewrite.old,
                rewrite.new,
                rewrite.file,
                anchor_suffix,
            ));
            if !dry_run && !changed.contains(&rewrite.file) {
                changed.push(rewrite.file.clone());
            }
        } else {
            log.warn(&format!(
                "enrolled version_files entry {} did not contain version {} (nothing rewritten)",
                rewrite.file, rewrite.old
            ));
        }
    }
    Ok(changed)
}

/// Build the deduped, conflict-checked set of `version_files` rewrites across
/// every per-crate group, in first-seen file order (and, within one file,
/// longest-`old`-first — the order the engine applies them in, so the logged
/// plan is the applied plan).
///
/// Two entries INTERACT when they name the same file and either share a `match`
/// anchor or one of them is bare (a bare entry sweeps the whole file, so it
/// overlaps every anchored region in it). Interacting entries are refused in
/// exactly two shapes, because each is a rewrite the file cannot survive:
///
/// 1. **Same old, two news** — one literal cannot become two different versions.
/// 2. **A chain** — one entry's NEW version is still matched by another's OLD
///    matcher, so the second rewrite would consume the first's output. No apply
///    order fixes it, so it is refused rather than reordered.
///
/// Distinct old versions that do not chain both rewrite: the engine applies
/// each pair to its own occurrences. Fully identical entries dedupe to a single
/// rewrite (lockstep crates share one pair, so they never conflict). On a
/// refusal this `bail!`s naming the file, the anchor overlap, and both crates.
///
/// Runs identically for dry-run and real tagging so the preview matches the
/// outcome — the validated plan is computed once, then either previewed or
/// applied by the caller.
pub(crate) fn plan_version_files_rewrites(
    tag_results: &[GroupTagResult],
) -> Result<Vec<VersionFileRewrite>> {
    let mut units: Vec<PlanUnit<'_>> = Vec::new();
    for group_result in tag_results {
        let Some(ref old) = group_result.old_version else {
            continue;
        };
        let owner = group_result
            .crate_names
            .first()
            .map(String::as_str)
            .unwrap_or("?");
        for ((_, new_version), files) in group_result
            .version_updates
            .iter()
            .zip(group_result.crate_version_files.iter())
        {
            units.push(PlanUnit {
                files,
                old,
                new: new_version,
                owner,
            });
        }
    }
    build_version_files_plan(&units)
}

/// One enrolling unit: an entry list under a single `old` → `new` bump, owned
/// by the named crate (or project).
pub(crate) struct PlanUnit<'a> {
    pub files: &'a [anodizer_core::config::VersionFileEntry],
    pub old: &'a str,
    pub new: &'a str,
    pub owner: &'a str,
}

/// The one `version_files` plan builder: dedupe, guard, order.
///
/// Every config mode funnels through here so the conflict guard cannot hold in
/// one mode and not another. A single unit is the single-crate and lockstep
/// shape; several units are the per-crate shape.
fn build_version_files_plan(units: &[PlanUnit<'_>]) -> Result<Vec<VersionFileRewrite>> {
    let mut plan: Vec<VersionFileRewrite> = Vec::new();
    for unit in units {
        for entry in unit.files {
            let candidate = VersionFileRewrite {
                file: entry.path().to_string(),
                anchor: entry.anchor().map(str::to_string),
                old: unit.old.to_string(),
                new: unit.new.to_string(),
                owner: unit.owner.to_string(),
            };
            if plan.iter().any(|p| {
                p.file == candidate.file
                    && p.anchor == candidate.anchor
                    && p.old == candidate.old
                    && p.new == candidate.new
            }) {
                continue;
            }
            for existing in &plan {
                check_rewrite_pair(existing, &candidate)?;
            }
            plan.push(candidate);
        }
    }
    sort_plan(&mut plan);
    Ok(plan)
}

/// The `version_files` enrollment the repo-level (no `--crate`) tag path owns,
/// resolved exactly as `anodizer check version-files` resolves it.
///
/// `check` builds one unit per configured crate through
/// [`resolve_version_files`], so a crate that enrolls nothing of its own is
/// checked against the TOP-LEVEL list at that crate's version. The repo-level
/// tag path serves the same shapes — no `crates:` block at all, or a single
/// declared crate tagged without `--crate` — so it resolves through the same
/// seam and rewrites what `check` validates.
///
/// Several declared crates reach the lockstep or per-crate engine, each of
/// which resolves its own list through that same seam, so this returns nothing
/// rather than sweeping the files a second time.
pub(crate) fn top_level_version_files(
    config: &anodizer_core::config::Config,
) -> Vec<anodizer_core::config::VersionFileEntry> {
    match config.crate_universe().as_slice() {
        [] => resolve_version_files(None, Some(config)),
        [single] => resolve_version_files(Some(single), Some(config)),
        _ => Vec::new(),
    }
}

/// Rewrite the top-level `version_files` of a config that declares no
/// `crates:` block, and — outside dry-run — commit what changed.
///
/// Such a config has no crate manifest for `tag` to version-sync, so nothing
/// else in the bump would touch its enrollment; `check version-files` reads the
/// same list, so leaving it unrewritten reports drift no bump could clear. The
/// rewrite goes through the same plan/apply seam as the `--crate` case, so one
/// file is swept once. Absent a previous tag there is no old version to rewrite
/// from and this is a no-op.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bump_top_level_version_files(
    root: &Path,
    files: &[anodizer_core::config::VersionFileEntry],
    old_tag: &str,
    new_version: &str,
    project_name: &str,
    dry_run: bool,
    skip_ci_suffix: &str,
    log: &StageLogger,
) -> Result<()> {
    let Some(old) = git::version_from_tag(old_tag) else {
        return Ok(());
    };
    let plan = version_files_plan(files, &old, new_version, project_name)?;
    let changed = rewrite_and_stage_version_files(root, &plan, dry_run, log)?;
    if dry_run || changed.is_empty() {
        return Ok(());
    }
    let staged: Vec<&str> = changed.iter().map(String::as_str).collect();
    git::stage_and_commit_in(
        root,
        &staged,
        &git::release_bump_subject(&format!("{project_name} → {new_version}"), skip_ci_suffix),
    )?;
    Ok(())
}

/// Build the deduped, conflict-checked plan for ONE enrollment list under a
/// single `old` → `new` bump: the single-crate (`--crate`), no-`crates:` and
/// lockstep-workspace shape. `owner` names the crate (or the project) in the
/// unmatched-anchor error.
///
/// One `(old, new)` pair does NOT make the file safe: when `new` still matches
/// `old`'s matcher — any prerelease target such as `1.2.3` → `1.2.3-rc1` — a
/// bare entry and an anchored entry on the same file rewrite the same bytes
/// twice, so the same guard [`plan_version_files_rewrites`] applies runs here.
pub(crate) fn version_files_plan(
    files: &[anodizer_core::config::VersionFileEntry],
    old: &str,
    new: &str,
    owner: &str,
) -> Result<Vec<VersionFileRewrite>> {
    build_version_files_plan(&[PlanUnit {
        files,
        old,
        new,
        owner,
    }])
}

/// Order the plan the way the engine applies it: first-seen file order, and
/// within one file the longest `old` first — a shorter `old` that is a
/// word-boundary prefix of a longer one would otherwise consume it (`0.1.0`
/// inside `0.1.0-rc1`). `sort_by_key` is stable, so entries that tie keep their
/// enrollment order.
fn sort_plan(plan: &mut [VersionFileRewrite]) {
    let mut file_order: Vec<String> = Vec::new();
    for rewrite in plan.iter() {
        if !file_order.contains(&rewrite.file) {
            file_order.push(rewrite.file.clone());
        }
    }
    plan.sort_by_key(|r| {
        (
            file_order.iter().position(|f| f == &r.file).unwrap_or(0),
            std::cmp::Reverse(r.old.len()),
        )
    });
}

/// Refuse the two rewrite shapes one file cannot survive, for a pair of
/// enrollments that overlap in it. Returns `Ok(())` when the pair does not
/// interact, or interacts safely (distinct olds, no chain).
fn check_rewrite_pair(a: &VersionFileRewrite, b: &VersionFileRewrite) -> Result<()> {
    if a.file != b.file {
        return Ok(());
    }
    // A bare entry sweeps the whole file, so it overlaps every anchored region
    // in it; two different anchors are asserted disjoint by their author.
    let anchor_suffix = match (a.anchor.as_deref(), b.anchor.as_deref()) {
        (None, None) => String::new(),
        (Some(x), Some(y)) if x == y => format!(" (match {x})"),
        (Some(_), Some(_)) => return Ok(()),
        (Some(anchor), None) | (None, Some(anchor)) => {
            format!(" (whole-file entry overlaps match {anchor})")
        }
    };

    if a.old == b.old && a.new != b.new {
        bail!(
            "version_files conflict: {}{} is enrolled by crates bumping FROM the same version to \
             different versions ({} {} → {} vs {} {} → {}); a file cannot hold two new versions \
             for one old one",
            a.file,
            anchor_suffix,
            a.owner,
            a.old,
            a.new,
            b.owner,
            b.old,
            b.new,
        );
    }

    // A chain is matcher-based, not string equality: `0.1.0` is found inside
    // `0.1.0-rc2` by the engine's word-boundary matcher, so `0.1.0-rc1 →
    // 0.1.0-rc2` beside `0.1.0 → 0.2.0` corrupts the first rewrite's output.
    let uses = anodizer_core::version_files::contains_version;
    let chain = if uses(&a.new, &b.old)? {
        Some((a, b))
    } else if uses(&b.new, &a.old)? {
        Some((b, a))
    } else {
        None
    };
    if let Some((first, second)) = chain {
        bail!(
            "version_files conflict: {}{} is enrolled by crates whose bumps chain ({} {} → {} \
             then {} {} → {}); the second rewrite would consume the first's output — give each \
             enrollment its own `match` anchor",
            a.file,
            anchor_suffix,
            first.owner,
            first.old,
            first.new,
            second.owner,
            second.old,
            second.new,
        );
    }

    Ok(())
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
