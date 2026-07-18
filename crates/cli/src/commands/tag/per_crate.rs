use super::*;

/// Compute tag results for all per-crate groups, performing change detection.
///
/// Returns the list of groups that need tagging, skipping groups with no
/// changes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_per_crate_tags(
    workspace_root: &Path,
    groups: &[Vec<CrateConfig>],
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    preloaded_config: Option<&anodizer_core::config::Config>,
    remote_tags: Option<&std::collections::HashSet<String>>,
    log: &StageLogger,
) -> Result<Vec<GroupTagResult>> {
    use crate::commands::release::detect_changed_crates_pub;

    // Use the already-loaded config when available to avoid a redundant disk
    // read; fall back to a fresh load, then to an empty default for fixture
    // repos that have no config file (e.g. integration-test temp dirs).
    let fallback: anodizer_core::config::Config;
    let anodizer_config: &anodizer_core::config::Config = if let Some(c) = preloaded_config {
        c
    } else {
        // A malformed explicitly-resolved config must fail, not silently
        // fall back to an empty default (the old `.ok().unwrap_or_default()`);
        // the default is reserved for fixture repos with genuinely no config.
        fallback = match resolve_config_path(opts) {
            Some(p) => crate::pipeline::load_config(&p)?,
            None => anodizer_core::config::Config::default(),
        };
        &fallback
    };

    // Run change detection across ALL crates so depends_on propagation works.
    let all_known: Vec<CrateConfig> = anodizer_config
        .crate_universe()
        .into_iter()
        .cloned()
        .collect();
    let changed_names = detect_changed_crates_pub(
        workspace_root,
        &all_known,
        anodizer_config.git.as_ref(),
        anodizer_config.monorepo_tag_prefix(),
        log,
    )?;

    if changed_names.is_empty() {
        return Ok(vec![]);
    }

    use std::collections::HashSet;
    let changed_set: HashSet<&str> = changed_names.iter().map(|s| s.as_str()).collect();

    let mut results: Vec<GroupTagResult> = Vec::new();

    for group in groups {
        // A group is selected if any of its crates appears in changed_names.
        let group_selected = group.iter().any(|c| changed_set.contains(c.name.as_str()));
        if !group_selected {
            continue;
        }

        let first = &group[0];
        let tag_prefix = git::extract_tag_prefix(first.tag_template.as_deref().unwrap_or(""))
            .unwrap_or_else(|| cfg.tag_prefix.clone());

        // Determine the previous tag for this group (use first crate's template).
        // Per-group only the tag_prefix (from this group's template) and
        // custom_tag (never applies in per-crate dispatch) differ from `cfg`.
        let group_cfg = ResolvedConfig {
            tag_prefix: tag_prefix.clone(),
            custom_tag: None,
            ..cfg.clone()
        };

        let prev_tag = find_previous_tag(&group_cfg, git_config, remote_tags)?;

        // Scan commits across all paths in the group.
        let mut all_messages: Vec<String> = Vec::new();
        for crate_cfg in group {
            // Propagate a failed scan: swallowing it here turned a git error
            // into "no bump signal" and a silent no-release at default
            // verbosity. `group_cfg` (not `cfg`) matches the sibling
            // `detect_bump_demoted` call; only `branch_history` is read.
            let msgs = get_messages_for_bump(
                workspace_root,
                &group_cfg,
                prev_tag.as_deref(),
                Some(&crate_cfg.path),
            )?;
            all_messages.extend(msgs);
        }
        let bump = detect_bump_demoted(&all_messages, &group_cfg, prev_tag.as_deref());

        // The group's own manifest version drives the same Cargo-ahead model
        // the lockstep path applies: a manifest strictly ahead of the previous
        // tag is a release signal, and one ahead of the tag-derived version
        // wins the derivation (so a first tag starts from the crate's real
        // `[package].version`, not the `initial_version` sentinel).
        let group_cargo_ver = group_manifest_version(group, workspace_root);
        let cargo_ahead = manifest_version_ahead(
            group_cargo_ver.as_deref(),
            prev_tag
                .as_deref()
                .and_then(|t| git::parse_semver_tag(t).ok())
                .map(|p| (p.major, p.minor, p.patch)),
        );

        if bump == BumpKind::None && !cargo_ahead {
            log.verbose(&format!(
                "skipped group {:?} — no bump signal and Cargo.toml not ahead",
                group.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
            ));
            continue;
        }

        let (new_major, new_minor, new_patch, old_tag_str) = if let Some(ref prev) = prev_tag {
            let base = git::parse_semver_tag(prev)?;
            let (maj, min, pat) = apply_bump(base.major, base.minor, base.patch, &bump);
            (maj, min, pat, prev.as_str())
        } else {
            let base = git::parse_semver_tag(&format!("{}{}", tag_prefix, cfg.initial_version))
                .unwrap_or(git::SemVer {
                    major: 0,
                    minor: 1,
                    patch: 0,
                    prerelease: None,
                    build_metadata: None,
                });
            (base.major, base.minor, base.patch, "")
        };

        let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);
        if cfg.prerelease {
            new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
        }

        // Cargo.toml-ahead guard, mirroring the lockstep path exactly: a
        // manifest version strictly ahead of the tag-derived one wins, so
        // autotag never downgrades a manual bump.
        if let Some(cargo_ver) = group_cargo_ver
            && manifest_version_ahead(Some(&cargo_ver), Some((new_major, new_minor, new_patch)))
        {
            log.status(&format!(
                "Cargo.toml version {} > tag-derived {}, using Cargo.toml version",
                cargo_ver, new_version
            ));
            new_version = cargo_ver;
        }

        log.verbose(&format!(
            "group {:?}: {} → {}{}",
            group.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            old_tag_str,
            tag_prefix,
            new_version
        ));

        // Build per-crate tags and version updates.
        let mut new_tags: Vec<(String, String)> = Vec::new();
        let mut version_updates: Vec<(String, String)> = Vec::new();
        let mut crate_version_files: Vec<Vec<String>> = Vec::new();
        for crate_cfg in group {
            let crate_prefix =
                git::extract_tag_prefix(crate_cfg.tag_template.as_deref().unwrap_or(""))
                    .unwrap_or_else(|| tag_prefix.clone());
            let new_tag = format!("{}{}", crate_prefix, new_version);
            let message = format!("Release {}", new_tag);
            new_tags.push((new_tag, message));
            version_updates.push((crate_cfg.path.clone(), new_version.clone()));
            crate_version_files.push(resolve_version_files(
                Some(crate_cfg),
                Some(anodizer_config),
            ));
        }

        results.push(GroupTagResult {
            crate_names: group.iter().map(|c| c.name.clone()).collect(),
            new_tags,
            version_updates,
            old_version: git::version_from_tag(old_tag_str),
            prev_tag: (!old_tag_str.is_empty()).then(|| old_tag_str.to_string()),
            crate_version_files,
        });
    }

    Ok(results)
}
/// Template vars + process env every tag hook surface provides: `Tag`,
/// `PrefixedTag`, `Version`, `PreviousTag` plus `ANODIZER_CURRENT_TAG` /
/// `ANODIZER_PREVIOUS_TAG`. One helper serves both the single/lockstep
/// `create_tag` closure and the per-crate group loop so the two surfaces
/// cannot drift. `ANODIZER_PREVIOUS_TAG` is REMOVED when there is no
/// previous tag: the per-crate loop reuses one process env across groups,
/// and a stale value left over from an earlier group would hand hooks a
/// wrong-crate tag range.
pub(crate) fn tag_hook_context(tag: &str, version: &str, prev: Option<&str>) -> TemplateVars {
    let mut tv = TemplateVars::new();
    tv.set("Tag", tag);
    tv.set("PrefixedTag", tag);
    tv.set("Version", version);
    if let Some(p) = prev {
        tv.set("PreviousTag", p);
    }
    // SAFETY: the tag subcommand runs single-threaded — no worker threads
    // exist here, so mutating the process env is safe. Hooks read these
    // via their subprocess environment.
    unsafe {
        std::env::set_var("ANODIZER_CURRENT_TAG", tag);
        match prev {
            Some(p) => std::env::set_var("ANODIZER_PREVIOUS_TAG", p),
            None => std::env::remove_var("ANODIZER_PREVIOUS_TAG"),
        }
    }
    tv
}

/// Run `tag_pre_hooks` / `tag_post_hooks` for one per-crate group, providing
/// the same template vars and process env as the single/lockstep `create_tag`
/// closure (via [`tag_hook_context`]). Runs once per UNIQUE tag: the group's
/// first tag stands in for the group (all its tags share one version), and
/// `fired` dedupes across groups — same-prefix groups bumping to the same
/// version resolve to ONE shared tag (mirroring the tag-creation dedup), so
/// its hooks must fire exactly once.
pub(crate) fn run_group_tag_hooks(
    hooks: &[anodizer_core::config::HookEntry],
    label: &str,
    group: &GroupTagResult,
    dry_run: bool,
    fired: &mut Vec<String>,
    log: &StageLogger,
) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    let Some((tag, _)) = group.new_tags.first() else {
        return Ok(());
    };
    if fired.iter().any(|t| t == tag) {
        return Ok(());
    }
    fired.push(tag.clone());
    // Version derives from the tag itself (family prefix stripped), matching
    // the single/lockstep closure's semantics; the group's version_updates
    // back-stop a tag with no extractable version.
    let version = git::version_from_tag(tag).unwrap_or_else(|| {
        group
            .version_updates
            .first()
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    });
    let tv = tag_hook_context(tag, &version, group.prev_tag.as_deref());
    run_hooks(hooks, label, HookRunContext::new(dry_run, log, Some(&tv)))
}

pub(crate) fn run_per_crate_tag(
    dispatch: PerCrateDispatch,
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    anodizer_config: Option<&anodizer_core::config::Config>,
    controls: PushControls<'_>,
    log: &StageLogger,
) -> Result<()> {
    let PerCrateDispatch {
        groups,
        is_flat_aggregate,
        workspace_root,
    } = dispatch;
    let cwd = workspace_root.clone();
    let tag_results = compute_per_crate_tags(
        &workspace_root,
        &groups,
        opts,
        cfg,
        git_config,
        anodizer_config,
        controls.remote_tags,
        log,
    )?;

    if tag_results.is_empty() {
        log.verbose("no changed crates — nothing to tag");
        println!("anodizer-output crates=[]");
        println!("anodizer-output versions={{}}");
        return Ok(());
    }

    let all_tagged_crates: Vec<String> = tag_results
        .iter()
        .flat_map(|r| r.crate_names.iter().cloned())
        .collect();

    let all_version_updates: Vec<(String, String)> = tag_results
        .iter()
        .flat_map(|r| r.version_updates.iter().cloned())
        .collect();

    // Dedupe across groups: same-prefix crates bumping to the same version
    // resolve to ONE shared tag, which must be created and pushed exactly once.
    let mut all_new_tags: Vec<String> = Vec::new();
    for r in &tag_results {
        for (t, _) in &r.new_tags {
            if !all_new_tags.contains(t) {
                all_new_tags.push(t.clone());
            }
        }
    }

    // Conflict-check + dedupe the version_files rewrites BEFORE the
    // dry-run/real split so a conflicting config bails identically in both
    // modes (and before any manifest is touched in the real run).
    let vf_plan = plan_version_files_rewrites(&tag_results)?;

    // One changelog target per bumped crate across all groups, each rendered
    // from its OWN group's previous tag to ITS new version. Empty when the
    // refresh is disabled.
    let mut changelog_targets = if controls.changelog_enabled {
        plan_changelog_targets(&cwd, &tag_results)
    } else {
        Vec::new()
    };
    // The real packaged crates behind the targets, captured BEFORE the flat-
    // aggregate collapse below replaces them with one synthetic project-label
    // target — provenance markers must name publishable crates only.
    let marker_crates: Vec<(String, PathBuf, String)> = changelog_targets
        .iter()
        .map(|t| {
            (
                t.crate_name.clone(),
                t.crate_dir.clone(),
                t.to_version.clone(),
            )
        })
        .collect();
    // Per-crate(multi-track) targets already carry distinct correct tags, so a
    // single routed call covers both destinations (no aggregate split needed).
    let cl_config = changelog_config_for(anodizer_config);
    let mut changelog_routing = ChangelogRouting::from_config(&cl_config);

    // Collapse the per-crate targets to ONE flat aggregate when this group is a
    // `FlatAggregate` (a flat `crates:` list whose members share one tag track,
    // bumped to N identically-prefixed tags) routed to one shared root: it is a
    // single lockstep release, not N multi-track subsections. Promoting each
    // member's section under the same `## [v<X.Y.Z>]` heading would strand every
    // member after the first and graft spurious `### <crate>` subsections — the
    // same bug the `changelog` command collapses. Genuine multi-track
    // (`PerCrate`) or per-crate files keep their per-crate targets.
    if collapse_targets_to_flat_aggregate(
        &mut changelog_targets,
        &cwd,
        anodizer_config,
        is_flat_aggregate && changelog_routing.root_enabled && !changelog_routing.per_crate,
    ) {
        changelog_routing.single_track = true;
    }

    // Genuine multi-track root: each target owns a `### <crate>` subsection.
    // Multi-track-ness is a property of the repo TOPOLOGY — more than one crate
    // routed to the shared root — not of how many tracks bump in this run. A
    // PerCrate release bumps one crate per tag, yet that crate's section still
    // belongs in the shared root as a tag-prefixed `## [<tag>]` section promoted
    // from its `### <crate>` subsection; gating on the per-run bump count would
    // drop every single-crate release to the flat roll (bare `## [<version>]`
    // headings that collide across tracks). Gate on the full root-routed crate
    // count from the one shared `config_root_crate_names` source — also the
    // crate-name set the renderer uses to bootstrap and classify subsections —
    // so this matches the refresh path and the two cannot diverge.
    changelog_routing.root_crate_names = anodizer_config
        .map(|cfg| {
            crate::commands::changelog_sync::config_root_crate_names(
                cfg,
                changelog_routing.root_crates,
            )
        })
        .unwrap_or_default();
    changelog_routing.multitrack = changelog_routing.root_enabled
        && !changelog_routing.single_track
        && changelog_routing.root_crate_names.len() > 1;

    if !opts.dry_run {
        // Apply version bumps across all changed crates in a single commit.
        // Crate paths are repo-root-relative; resolve each against the
        // discovered workspace root so the manifest IO matches the git working
        // dir even when `tag` runs from a subdirectory.
        for (path, new_version) in &all_version_updates {
            let abs_crate_dir = workspace_root.join(path).to_string_lossy().into_owned();
            anodizer_stage_build::version_sync::sync_version(
                &abs_crate_dir,
                new_version,
                false,
                log,
            )?;
        }

        // Propagate every bumped crate's new version into sibling manifests'
        // intra-workspace `[dependencies].<crate>.version` pins. Without this,
        // a workspace member that depends on a sibling at the old pinned
        // version (e.g. `cfgd-core = { path = "../cfgd-core", version = "0.3.5" }`)
        // still references the pre-bump version after sync_version() rewrites
        // only `[package].version`, and `cargo publish` later fails with
        // "failed to select a version for the requirement <sibling> = ^<old>".
        //
        // Each propagation is scoped to the Cargo workspace that owns the
        // bumped crate, so a bump in one release group never rewrites a pin in
        // an independent group whose crates live in a separate Cargo workspace.
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let mut intra_ws_modified: Vec<String> = Vec::new();
        for group_result in &tag_results {
            for (crate_name, (crate_path, new_version)) in group_result
                .crate_names
                .iter()
                .zip(group_result.version_updates.iter())
            {
                let abs_crate_dir = workspace_root
                    .join(crate_path)
                    .to_string_lossy()
                    .into_owned();
                let modified = anodizer_stage_build::version_sync::sync_workspace_deps(
                    &workspace_root_str,
                    &abs_crate_dir,
                    crate_name,
                    new_version,
                    false,
                    log,
                )?;
                intra_ws_modified.extend(modified);
            }
        }

        // Update Cargo.lock to match bumped manifests.
        match anodizer_core::cargo_lock::cargo_update_workspace(Some(workspace_root.as_path())) {
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

        // Stage all bumped Cargo.toml files + intra-workspace dep rewrites +
        // Cargo.lock. Convert absolute intra-ws paths to repo-relative so
        // `git add` recognizes them.
        let mut files_to_stage: Vec<String> = all_version_updates
            .iter()
            .map(|(path, _)| format!("{}/Cargo.toml", path))
            .collect();
        for abs in &intra_ws_modified {
            let rel = Path::new(abs)
                .strip_prefix(&workspace_root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| abs.clone());
            if !files_to_stage.contains(&rel) {
                files_to_stage.push(rel);
            }
        }
        // Rewrite enrolled version_files using each crate's group old→new.
        // The plan is conflict-checked (a shared path with non-identical
        // (old,new) pairs bails) and deduped once, identically to the dry-run
        // branch, so the preview matches the real run.
        for rewrite in &vf_plan {
            let vf_changed = rewrite_and_stage_version_files(
                &workspace_root,
                std::slice::from_ref(&rewrite.file),
                &rewrite.old,
                &rewrite.new,
                false,
                log,
            )?;
            for f in vf_changed {
                if !files_to_stage.contains(&f) {
                    files_to_stage.push(f);
                }
            }
        }

        // Refresh each bumped crate's CHANGELOG.md and fold the written
        // (repo-relative) paths into the same bump commit.
        let cl_changed =
            render_and_stage_changelogs(&cwd, &changelog_targets, &changelog_routing, false, log)?;
        for f in &cl_changed {
            if !files_to_stage.contains(f) {
                files_to_stage.push(f.clone());
            }
        }

        files_to_stage.push("Cargo.lock".to_string());
        let staged_refs: Vec<&str> = files_to_stage.iter().map(|s| s.as_str()).collect();

        // Build per-crate version arrows for the commit subject so each
        // crate's new version is visible (core→1.1.0, cli→2.1.0) instead of
        // using a single version that may only be correct for one group.
        let version_arrows: Vec<String> = tag_results
            .iter()
            .flat_map(|r| r.version_updates.iter())
            .map(|(path, ver)| {
                // Use the last path component as a short label.
                let label = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str());
                format!("{}→{}", label, ver)
            })
            .collect();
        let bump_summary = if version_arrows.is_empty() {
            all_tagged_crates.join(", ")
        } else {
            version_arrows.join(", ")
        };
        // Markers derived from the actually-written paths per REAL crate: a
        // crate earns one only when its own crate-root CHANGELOG.md was
        // regenerated, so a shared-root-only write never vouches for member
        // files the tool did not touch, and one crate's regeneration never
        // vouches for a same-numbered version of a sibling.
        let cl_markers = crate::commands::changelog_sync::changelog_provenance_markers(
            &cwd,
            &marker_crates,
            &cl_changed,
        );
        git::stage_and_commit_in(
            &workspace_root,
            &staged_refs,
            &crate::commands::changelog_sync::commit_message_with_markers(
                git::release_bump_subject(&bump_summary, skip_ci_suffix(cfg.skip_ci_on_bump)),
                &cl_markers,
            ),
        )?;

        // Pre hooks run once per unique tag, after the bump commit and before
        // the group's tags exist — the same point in the lifecycle at which
        // the single/lockstep closure runs them.
        let mut pre_fired: Vec<String> = Vec::new();
        for group_result in &tag_results {
            run_group_tag_hooks(
                controls.pre_hooks,
                "tag-pre",
                group_result,
                false,
                &mut pre_fired,
                log,
            )?;
        }

        // Create all tags locally; push happens atomically below. Crates that
        // share one tag prefix AND bump to the same version resolve to the SAME
        // tag (a lockstep aggregate expressed as a flat `crates:` list); creating
        // it once per crate would fail the second with "tag already exists", so
        // dedupe to one creation per distinct tag.
        let mut created: Vec<&str> = Vec::new();
        for group_result in &tag_results {
            for (tag, message) in &group_result.new_tags {
                if created.contains(&tag.as_str()) {
                    continue;
                }
                git::create_tag_local_only(&cwd, tag, message, false, controls.sign, log)?;
                created.push(tag.as_str());
            }
        }
    } else {
        // Dry-run: preview the same conflict-checked, deduped rewrite plan
        // without touching disk.
        for rewrite in &vf_plan {
            rewrite_and_stage_version_files(
                &workspace_root,
                std::slice::from_ref(&rewrite.file),
                &rewrite.old,
                &rewrite.new,
                true,
                log,
            )?;
        }
        render_and_stage_changelogs(&cwd, &changelog_targets, &changelog_routing, true, log)?;
        // Dry-run previews the pre hooks too, matching the single/lockstep
        // closure (which invokes run_hooks in dry mode).
        let mut pre_fired: Vec<String> = Vec::new();
        for group_result in &tag_results {
            run_group_tag_hooks(
                controls.pre_hooks,
                "tag-pre",
                group_result,
                true,
                &mut pre_fired,
                log,
            )?;
        }
        // Preview the tag creations too, one line per distinct tag, matching
        // the single/lockstep closure. Routing through create_tag_local_only in
        // dry-run mode reuses its exact `(dry-run) would create local [signed]
        // tag …` wording and honors the resolved sign flag; it creates nothing.
        let mut previewed: Vec<&str> = Vec::new();
        for group_result in &tag_results {
            for (tag, message) in &group_result.new_tags {
                if previewed.contains(&tag.as_str()) {
                    continue;
                }
                git::create_tag_local_only(&cwd, tag, message, true, controls.sign, log)?;
                previewed.push(tag.as_str());
            }
        }
    }

    // Build the structured-output payloads up front, but DON'T print them
    // yet: a downstream consumer treats `anodizer-output crates=…` as "these
    // crates are tagged and pushed". Emitting before the atomic push would
    // advertise a successful tagging even when the push then fails (the `?`
    // below aborts mid-command, leaving the consumer believing the tags
    // landed). Defer the `println!`s until after the push returns Ok.
    let crates_json =
        serde_json::to_string(&all_tagged_crates).unwrap_or_else(|_| "[]".to_string());

    // Build crate-name → new-version map from version_updates (path → version),
    // joined against crate_names so the output uses canonical crate names rather
    // than filesystem paths. Each group's crates share the same new version.
    // BTreeMap so the emitted JSON key order is stable across runs — CI logs
    // and doc examples must not flicker on HashMap iteration order.
    let versions_map: std::collections::BTreeMap<String, String> = tag_results
        .iter()
        .flat_map(|r| {
            r.crate_names
                .iter()
                .zip(r.version_updates.iter())
                .map(|(name, (_, ver))| (name.clone(), ver.clone()))
        })
        .collect();
    let versions_json = serde_json::to_string(&versions_map).unwrap_or_else(|_| "{}".to_string());

    // Per-crate auto-dispatch shares the fully-local default: a bare run pushes
    // nothing. `--push` / `tag.push=true` opts into the atomic branch+tags push;
    // `--push-tags-only` opts into the deferred-branch pattern (push the tags
    // now, advance the branch after publish succeeds); `--push-dry-run` previews
    // the atomic push without executing it. Pushing tags without their bump
    // commit (an orphan tag) is reachable only through explicit `--push-tags-only`.
    let push_dry = opts.dry_run || opts.push_dry_run;
    let push_branch = if opts.push_tags_only {
        None
    } else if resolve_effective_push(opts, controls.config_push) || opts.push_dry_run {
        // `--push-dry-run` selects the atomic branch+tags push even without
        // `--push`, mirroring the single/lockstep path; the push leg below then
        // runs in preview mode via `push_dry`.
        Some(git::get_current_branch()?)
    } else {
        None
    };
    if push_branch.is_some() || opts.push_tags_only {
        git::push_branch_and_tags_atomic_in(
            &cwd,
            &git::AtomicPushSpec {
                remote: controls.remote,
                branch: push_branch.as_deref(),
                tags: &all_new_tags,
                dry_run: push_dry,
                strict: opts.strict,
            },
            log,
        )?;
    } else if !opts.dry_run {
        log.status(&format!(
            "created {} locally; nothing was pushed — \
             pass --push to push the bump commit + tags atomically",
            all_new_tags.join(", ")
        ));
    }

    // Push succeeded (or was a dry-run/preview no-op that returns Ok). Now it
    // is safe to advertise the tagged crates + versions — and it must happen
    // BEFORE the post hooks: the tags are live on the remote at this point, so
    // a failing post hook must not abort the command with the payload
    // unprinted (a CI consumer would read the missing payload as "nothing
    // tagged" while live tags exist). In dry-run the lines still appear so CI
    // can observe what would be tagged.
    println!("anodizer-output crates={}", crates_json);
    println!("anodizer-output versions={}", versions_json);

    // Post hooks run only after the push succeeded, mirroring the closure's
    // post-push placement (a failed push must not fire release-announce-style
    // post hooks). A post-hook failure still errors the command.
    let mut post_fired: Vec<String> = Vec::new();
    for group_result in &tag_results {
        run_group_tag_hooks(
            controls.post_hooks,
            "tag-post",
            group_result,
            opts.dry_run,
            &mut post_fired,
            log,
        )?;
    }

    Ok(())
}
