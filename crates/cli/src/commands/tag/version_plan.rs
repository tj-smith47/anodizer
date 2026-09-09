use super::*;

/// Apply every planned `version_files` rewrite, log the per-entry outcome, and
/// return the repo-relative paths that actually changed (so the caller can
/// stage them into the bump commit).
///
/// Enrolled paths are repo-root-relative and stay that way: the engine joins
/// each against `root` (the discovered workspace root) for the read/write so
/// the rewrite hits the same files git operates on even when `tag` is invoked
/// from a subdirectory, and every message — including the engine's own
/// unmatched-anchor error — names the relative path the user enrolled. The
/// returned paths are relative too, so staging via
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
            path: r.file.clone(),
            anchor: r.anchor.clone(),
            old: r.old.clone(),
            new: r.new.clone(),
            owner: r.owner.clone(),
        })
        .collect();
    let outcomes =
        anodizer_core::version_files::rewrite_version_in_files(root, &rewrites, dry_run)?;
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
/// every per-crate group, in first-seen file order and, within one file,
/// longest-`old`-first. That is the LOG order — the order the per-entry
/// outcome lines are printed in. The engine re-orders for correctness before
/// applying (anchored entries claim their regions before the bare sweep, and
/// only then longest-`old`-first), so a file carrying both a bare and an
/// anchored entry is applied in a different order than it is logged.
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
/// rewrite (lockstep crates share one pair, so they never conflict), and two
/// entries from ONE owner on ONE pair — a bare entry beside an anchored one —
/// are exempt from both hazards: they express a single rewrite, which the
/// claiming engine applies to each occurrence exactly once. On a
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
/// Both commands resolve their units through
/// [`crate::commands::version_files_resolve::enrolled_units`],
/// so a crate that enrolls nothing of its own is checked — and rewritten —
/// against the TOP-LEVEL list at that crate's version. The repo-level tag path
/// serves the shapes that resolve to a single unit: no `crates:` block at all,
/// or one declared crate tagged without `--crate`.
///
/// Several declared crates reach the lockstep or per-crate engine, each of
/// which resolves the same units for itself, so this returns nothing rather
/// than sweeping the files a second time.
pub(crate) fn top_level_version_files(
    config: &anodizer_core::config::Config,
) -> Vec<anodizer_core::config::VersionFileEntry> {
    if config.crate_universe().len() > 1 {
        return Vec::new();
    }
    crate::commands::version_files_resolve::enrolled_units(config)
        .into_iter()
        .flat_map(|unit| unit.files)
        .collect()
}

/// The manifest directory (repo-relative; `"."` for the repo root) the
/// repo-level tag path writes the new version into, or `None` when this arm
/// owns no manifest and must leave the tree alone.
///
/// `anodizer check version-files` compares every enrolled file against a
/// MANIFEST version, so an arm that rewrote enrolled files without moving the
/// manifest would leave a drift report no bump could ever clear. The arm
/// therefore writes the same manifest `check` reads: the repo root's when no
/// crate is declared, the single declared crate's when one is. A declared crate
/// that has not opted into `version_sync` owns no manifest write here — exactly
/// as under `--crate`, where the enrolled files are not rewritten either.
///
/// Several declared crates never reach this arm: they dispatch to the lockstep
/// or per-crate engine, each of which bumps its own manifests.
pub(crate) fn repo_level_manifest_dir(config: &Config) -> Option<String> {
    match config.crate_universe().as_slice() {
        [] => Some(".".to_string()),
        [single] => single
            .version_sync
            .as_ref()
            .and_then(|vs| vs.enabled)
            .unwrap_or(false)
            .then(|| single.path.clone()),
        _ => None,
    }
}

/// Everything the repo-level bump writes into one commit.
pub(crate) struct RepoLevelBump<'a> {
    /// Manifest directory from [`repo_level_manifest_dir`].
    pub manifest_dir: &'a str,
    pub files: &'a [anodizer_core::config::VersionFileEntry],
    pub old_tag: &'a str,
    pub new_version: &'a str,
    pub project_name: &'a str,
    pub dry_run: bool,
    pub skip_ci_suffix: &'a str,
}

/// The repo-level (no `--crate`, no lockstep workspace) bump: write
/// `new_version` into the manifest, rewrite the enrolled `version_files`, and
/// land both in one commit before the tag is created — the shape every other
/// tag path produces.
///
/// Refuses a manifest that declares no version rather than inventing a
/// `[package]` table: a repo whose root manifest carries no version has none
/// for `check version-files` to compare against either, so the enrollment is
/// the thing to fix. Absent a previous tag there is no old version to rewrite
/// the enrolled files from, and only the manifest moves.
pub(crate) fn bump_repo_level(
    root: &Path,
    bump: &RepoLevelBump<'_>,
    log: &StageLogger,
) -> Result<()> {
    let manifest_rel = if bump.manifest_dir == "." {
        "Cargo.toml".to_string()
    } else {
        format!("{}/Cargo.toml", bump.manifest_dir)
    };
    let manifest_dir = root.join(bump.manifest_dir).to_string_lossy().into_owned();
    if anodizer_stage_build::version_sync::read_cargo_version_opt(&manifest_dir)
        .unwrap_or(None)
        .is_none()
    {
        bail!(
            "version_files: the repo-level bump must write {} into a manifest, but {} declares              no [package].version and the workspace declares no [workspace.package].version;              give the manifest a version, declare the crate under `crates:`, or drop the              version_files enrollment",
            bump.new_version,
            manifest_rel,
        );
    }

    anodizer_stage_build::version_sync::sync_version(
        root,
        bump.manifest_dir,
        bump.new_version,
        bump.dry_run,
        log,
    )?;

    let mut changed: Vec<String> = Vec::new();
    if let Some(old) = git::version_from_tag(bump.old_tag) {
        let plan = version_files_plan(bump.files, &old, bump.new_version, bump.project_name)?;
        changed = rewrite_and_stage_version_files(root, &plan, bump.dry_run, log)?;
    }
    if bump.dry_run {
        return Ok(());
    }

    // A bumped Cargo.toml beside a stale Cargo.lock dirties the tree the moment
    // anything cargo-shaped runs against the tagged commit.
    let lockfile = root.join("Cargo.lock");
    if lockfile.is_file() {
        match anodizer_core::cargo_lock::cargo_update_workspace(Some(root)) {
            Ok(true) => {}
            Ok(false) => warn_cargo_lock_stale(
                log,
                "`cargo update --workspace` exited non-zero after version sync",
            ),
            Err(e) => warn_cargo_lock_stale(
                log,
                &format!("could not spawn `cargo update --workspace` ({e})"),
            ),
        }
    }

    let mut staged: Vec<&str> = vec![&manifest_rel];
    if lockfile.is_file() {
        staged.push("Cargo.lock");
    }
    for f in &changed {
        staged.push(f);
    }
    git::stage_and_commit_in(
        root,
        &staged,
        &git::release_bump_subject(
            &format!("{} → {}", bump.project_name, bump.new_version),
            bump.skip_ci_suffix,
        ),
    )?;
    Ok(())
}

/// The plan for a bump where every enrolled owner shares one `old` → `new` —
/// the lockstep shape — built by the same builder the per-crate and
/// single-crate paths use, so the guard and the dedupe behave identically.
pub(crate) fn shared_version_files_plan(
    units: &[crate::commands::version_files_resolve::EnrolledUnit],
    old: &str,
    new: &str,
) -> Result<Vec<VersionFileRewrite>> {
    let plan_units: Vec<PlanUnit<'_>> = units
        .iter()
        .map(|unit| PlanUnit {
            files: &unit.files,
            old,
            new,
            owner: &unit.owner,
        })
        .collect();
    build_version_files_plan(&plan_units)
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

/// How one side of a refusal is named: several owners are named by crate, a
/// single owner by the entry it wrote, because "crates" sends the author
/// looking for a second crate that does not exist.
fn refusal_side(rewrite: &VersionFileRewrite, same_owner: bool) -> String {
    match (same_owner, rewrite.anchor.as_deref()) {
        (false, _) => rewrite.owner.clone(),
        (true, Some(anchor)) => format!("match {anchor}"),
        (true, None) => "the whole-file entry".to_string(),
    }
}

/// Refuse the two rewrite shapes one file cannot survive, for a pair of
/// enrollments that overlap in it. Returns `Ok(())` when the pair does not
/// interact, or interacts safely (distinct olds, no chain).
fn check_rewrite_pair(a: &VersionFileRewrite, b: &VersionFileRewrite) -> Result<()> {
    if a.file != b.file {
        return Ok(());
    }
    // One owner rewriting one file at one pair is ONE rewrite, however many
    // entries express it: every entry selects its occurrences from the original
    // content and each occurrence is claimed once, so a bare entry beside an
    // anchored one on the same bump lands exactly the same bytes as either
    // alone. Only DIFFERENT pairs can chain.
    if a.owner == b.owner && a.old == b.old && a.new == b.new {
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

    let same_owner = a.owner == b.owner;
    let enrolled_by = if same_owner {
        format!("is enrolled twice by {}", a.owner)
    } else {
        "is enrolled by crates".to_string()
    };

    if a.old == b.old && a.new != b.new {
        bail!(
            "version_files conflict: {}{} {} bumping FROM the same version to \
             different versions ({} {} → {} vs {} {} → {}); a file cannot hold two new versions \
             for one old one",
            a.file,
            anchor_suffix,
            enrolled_by,
            refusal_side(a, same_owner),
            a.old,
            a.new,
            refusal_side(b, same_owner),
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
            "version_files conflict: {}{} {} whose bumps chain ({} {} → {} \
             then {} {} → {}); the second rewrite would consume the first's output — give each \
             enrollment its own `match` anchor",
            a.file,
            anchor_suffix,
            enrolled_by,
            refusal_side(first, same_owner),
            first.old,
            first.new,
            refusal_side(second, same_owner),
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
