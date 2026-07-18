use super::*;

/// Resolve the effective `changelog:` block (owned), falling back to the
/// default (root-only) when no config or no `changelog:` block is present, so
/// the routing decision is uniform across every tagging mode.
pub(crate) fn changelog_config_for(
    config: Option<&anodizer_core::config::Config>,
) -> anodizer_core::config::ChangelogConfig {
    config.and_then(|c| c.changelog.clone()).unwrap_or_default()
}

pub(crate) fn apply_workspace_bump(
    workspace_root: &Path,
    ws: &WorkspaceInfo,
    new_version: &str,
    edits: &WorkspaceBumpEdits<'_>,
    dry_run: bool,
    skip_ci_on_bump: bool,
    log: &StageLogger,
) -> Result<bool> {
    let WorkspaceBumpEdits { vf, cl } = edits;
    let rows: Vec<PlanRow> = ws
        .members
        .iter()
        .map(|m| {
            let current = if m.inherits_workspace_version {
                ws.workspace_package_version.clone().unwrap_or_default()
            } else {
                m.own_version.clone().unwrap_or_default()
            };
            let level = if current == new_version {
                BumpLevel::Skip
            } else {
                BumpLevel::Explicit
            };
            PlanRow {
                crate_name: m.name.clone(),
                current,
                next: new_version.to_string(),
                level,
                reason: "workspace tag".into(),
                edited_files: vec![],
                manifest: m.manifest_path.clone(),
                inherits_workspace_version: m.inherits_workspace_version,
            }
        })
        .collect();

    if rows.iter().all(|r| r.level == BumpLevel::Skip) {
        log.verbose(&format!(
            "workspace already at {}, nothing to sync",
            new_version
        ));
        return Ok(false);
    }

    // Lockstep splits the changelog destinations: per-crate files get one target
    // per member, but the shared root gets a SINGLE aggregate target. The members
    // all share the one workspace tag, so promoting per-member into the root would
    // strand every member after the first (the `## [tag]` heading already exists).
    // The aggregate target spans the whole workspace (`crate_dir = workspace_root`,
    // unfiltered) so the root section aggregates the entire release.
    let per_crate_targets: Vec<ChangelogTarget> = if cl.enabled && cl.routing.per_crate {
        ws.members
            .iter()
            .map(|m| ChangelogTarget {
                crate_name: m.name.clone(),
                crate_dir: m
                    .manifest_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| workspace_root.to_path_buf()),
                from_tag: cl.from_tag.map(str::to_string),
                to_version: new_version.to_string(),
                full_tag: cl.full_tag.to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let per_crate_routing = ChangelogRouting {
        root_enabled: false,
        per_crate: true,
        chronology: cl.routing.chronology,
        root_crates: cl.routing.root_crates,
        single_track: false,
        // Per-crate files are flat and independent; the root is handled by the
        // separate aggregate routing below.
        multitrack: false,
        root_crate_names: Vec::new(),
    };

    let root_aggregate_target: Vec<ChangelogTarget> = if cl.enabled && cl.routing.root_enabled {
        vec![ChangelogTarget {
            crate_name: ws
                .members
                .first()
                .map(|m| m.name.clone())
                .unwrap_or_default(),
            crate_dir: workspace_root.to_path_buf(),
            from_tag: cl.from_tag.map(str::to_string),
            to_version: new_version.to_string(),
            full_tag: cl.full_tag.to_string(),
        }]
    } else {
        Vec::new()
    };
    let root_routing = ChangelogRouting {
        root_enabled: true,
        per_crate: false,
        chronology: cl.routing.chronology,
        // The lockstep aggregate is one flat whole-release section (not a
        // per-crate `### subsection`), so the per-crate `root.crates` filter
        // must not gate it; filtering on the arbitrary first-member name would
        // silently drop the entire lockstep root changelog.
        root_crates: None,
        // One shared workspace tag over all members: the root holds one flat
        // whole-release block. Force the flat roll so a curated `[Unreleased]`
        // whose `### <Heading>` titles diverge from the configured `groups:`
        // is not misread as multi-track and grafted with a `### <crate>`.
        single_track: true,
        // A lockstep aggregate is flat, never multitrack.
        multitrack: false,
        root_crate_names: Vec::new(),
    };

    if dry_run {
        log.status(&format!(
            "(dry-run) would bump {} workspace crate(s) → {}",
            rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
            new_version
        ));
        if let Some(old) = vf.old {
            rewrite_and_stage_version_files(workspace_root, vf.files, old, new_version, true, log)?;
        }
        render_and_stage_changelogs(
            workspace_root,
            &per_crate_targets,
            &per_crate_routing,
            true,
            log,
        )?;
        render_and_stage_changelogs(
            workspace_root,
            &root_aggregate_target,
            &root_routing,
            true,
            log,
        )?;
        return Ok(false);
    }

    apply_plan(workspace_root, &rows, false, log)?;

    match anodizer_core::cargo_lock::cargo_update_workspace(Some(workspace_root)) {
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

    let mut staged: Vec<PathBuf> = Vec::new();
    let root_manifest = workspace_root.join("Cargo.toml");
    staged.push(root_manifest.clone());
    for m in &ws.members {
        if m.manifest_path != root_manifest && !staged.contains(&m.manifest_path) {
            staged.push(m.manifest_path.clone());
        }
    }
    let lockfile = workspace_root.join("Cargo.lock");
    if lockfile.is_file() {
        staged.push(lockfile);
    }

    let mut staged_rel: Vec<String> = staged
        .iter()
        .map(|p| {
            p.strip_prefix(workspace_root)
                .unwrap_or(p.as_path())
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // version_files are repo-root-relative already; rewrite the shared old→new
    // and fold the changed paths into the same bump commit.
    if let Some(old) = vf.old {
        let vf_changed = rewrite_and_stage_version_files(
            workspace_root,
            vf.files,
            old,
            new_version,
            false,
            log,
        )?;
        for f in vf_changed {
            if !staged_rel.contains(&f) {
                staged_rel.push(f);
            }
        }
    }

    // Refresh the per-crate files (one section per member) and the single
    // aggregate root section, folding every written (repo-relative) path into
    // the same bump commit.
    let mut cl_changed = render_and_stage_changelogs(
        workspace_root,
        &per_crate_targets,
        &per_crate_routing,
        false,
        log,
    )?;
    cl_changed.extend(render_and_stage_changelogs(
        workspace_root,
        &root_aggregate_target,
        &root_routing,
        false,
        log,
    )?);
    for f in &cl_changed {
        if !staged_rel.contains(f) {
            staged_rel.push(f.clone());
        }
    }

    let staged_refs: Vec<&str> = staged_rel.iter().map(|s| s.as_str()).collect();

    // Provenance markers derived from the actually-written paths across ALL
    // members (not just the per-crate targets): a root-only aggregate config
    // regenerates no member's own CHANGELOG.md and mints no marker, while a
    // member whose directory is the workspace root owns the root file and
    // does.
    let marker_crates: Vec<(String, PathBuf, String)> = ws
        .members
        .iter()
        .map(|m| {
            (
                m.name.clone(),
                m.manifest_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| workspace_root.to_path_buf()),
                new_version.to_string(),
            )
        })
        .collect();
    let cl_markers = crate::commands::changelog_sync::changelog_provenance_markers(
        workspace_root,
        &marker_crates,
        &cl_changed,
    );
    git::stage_and_commit_in(
        workspace_root,
        &staged_refs,
        &crate::commands::changelog_sync::commit_message_with_markers(
            git::release_bump_subject(
                &format!("workspace → {}", new_version),
                skip_ci_suffix(skip_ci_on_bump),
            ),
            &cl_markers,
        ),
    )?;

    log.status(&format!(
        "bumped {} workspace crate(s) → {}",
        rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
        new_version
    ));
    Ok(true)
}
