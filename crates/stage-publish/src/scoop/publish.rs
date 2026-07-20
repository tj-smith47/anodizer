use super::*;

// ---------------------------------------------------------------------------
// publish_to_scoop
// ---------------------------------------------------------------------------

/// Render and push the Scoop manifest for `crate_name`.
///
/// Returns `Ok(true)` when an actual git push was made to the bucket
/// repo; `Ok(false)` when the publish was skipped (skip_upload, dry-run,
/// or any future early-exit guard). The caller (Publisher::run) uses
/// the boolean to decide whether to record rollback evidence — see
/// `publish_to_homebrew` for the long-form rationale.
pub fn publish_to_scoop(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "scoop")?;

    let scoop_cfg = publish
        .scoop
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no scoop config for '{}'", crate_name))?;

    // Check skip_upload / `if:` gate before doing any work, matching the order
    // the shared renderer applies — so a skipped crate short-circuits before
    // repo resolution or the dry-run log line, exactly as before.
    let label = format!("scoop publisher for crate '{}'", crate_name);
    if util::should_skip_publisher_with_if(
        ctx,
        None,
        scoop_cfg.skip_upload.as_ref(),
        scoop_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(false);
    }

    let (repo_owner, repo_name) =
        crate::util::resolve_repo_owner_name(scoop_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("scoop: no repository config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Scoop bucket {}/{} for '{}'",
            repo_owner, repo_name, crate_name
        ));
        return Ok(false);
    }

    let version = ctx.version();

    // Use name override if set, otherwise crate name; render through template
    // engine. Recomputed here (cheap) because the manifest filename and commit
    // message key off the rendered name; the manifest body itself is rendered
    // by `render_scoop_manifest_for_crate`.
    let manifest_name_raw = scoop_cfg.name.as_deref().unwrap_or(crate_name);
    let manifest_name_rendered = util::render_or_warn(ctx, log, "scoop.name", manifest_name_raw)?;
    let manifest_name = manifest_name_rendered.as_str();

    // Render the manifest via the same path the schema validator uses. The
    // skip_upload / `if:` gate was already evaluated above; the renderer
    // re-checks it (returning None) but on this path it always yields Some.
    let Some(manifest) = render_scoop_manifest_for_crate(ctx, crate_name, log)? else {
        return Ok(false);
    };

    // Clone bucket repo, write manifest, commit, push.
    let token = util::resolve_repo_token(
        ctx,
        scoop_cfg.repository.as_ref(),
        Some("SCOOP_BUCKET_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        ctx,
        scoop_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "scoop",
        log,
    )?;

    // Place the manifest in the configured subdirectory, defaulting to
    // `bucket/`: scoop's `Find-BucketDirectory` resolves manifests ONLY from
    // `<repo>/bucket` when that directory exists, falling back to the repo
    // root when it doesn't — so `bucket/` works for both layouts, while a
    // root-level manifest is invisible the moment the repo carries a
    // `bucket/` dir. An explicit empty `directory: ""` targets the root.
    let dir = scoop_cfg.directory.as_deref().unwrap_or("bucket");
    let manifest_dir = if dir.is_empty() {
        repo_path.to_path_buf()
    } else {
        let d = repo_path.join(dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("scoop: create directory {}", d.display()))?;
        d
    };

    let manifest_path = manifest_dir.join(format!("{}.json", manifest_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    log.status(&format!("wrote Scoop manifest {}", manifest_path.display()));

    // A same-named manifest previously published at the repo ROOT is dead
    // weight once the subdirectory exists (scoop no longer resolves it) and
    // would contradict the copy just written — migrate it out in the same
    // commit.
    let stale_root_manifest = (manifest_dir != repo_path)
        .then(|| repo_path.join(format!("{}.json", manifest_name)))
        .filter(|p| p.is_file());
    if let Some(stale) = &stale_root_manifest {
        std::fs::remove_file(stale)
            .with_context(|| format!("scoop: remove stale root manifest {}", stale.display()))?;
        log.status(&format!(
            "removed stale root-level Scoop manifest {} (superseded by {})",
            stale.display(),
            manifest_path.display()
        ));
    }

    let scoop_default = "Scoop update for {{ ProjectName }} version {{ Tag }}";
    let commit_msg = crate::homebrew::render_commit_msg(
        Some(
            scoop_cfg
                .commit_msg_template
                .as_deref()
                .unwrap_or(scoop_default),
        ),
        manifest_name,
        &version,
        "manifest",
        log,
        ctx.render_is_strict(),
    )?;

    let mut commit_files: Vec<String> = vec![manifest_path.to_string_lossy().into_owned()];
    if let Some(stale) = &stale_root_manifest {
        // `git add` on a deleted tracked path stages the removal.
        commit_files.push(stale.to_string_lossy().into_owned());
    }
    let commit_file_refs: Vec<&str> = commit_files.iter().map(String::as_str).collect();
    let commit_opts = util::resolve_commit_opts(ctx, scoop_cfg.commit_author.as_ref(), log)?;
    let branch = util::resolve_branch_or_versioned(
        ctx,
        scoop_cfg.repository.as_ref(),
        manifest_name,
        &version,
    );
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &commit_file_refs,
        &commit_msg,
        branch.as_deref(),
        "scoop",
        &commit_opts,
        log,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Scoop bucket {}/{} updated for '{}'",
                repo_owner, repo_name, crate_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, scoop manifest for '{}' already up to date",
                manifest_name
            ));
        }
    }

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.as_deref().unwrap_or("main");
    // Clone the repository config so the `maybe_submit_pr` call no
    // longer borrows from `ctx.config` (via `scoop_cfg`). NLL then
    // drops the immutable borrow, making the subsequent `&mut ctx`
    // call legal.
    let repo_for_pr = scoop_cfg.repository.clone();
    let pr_outcome = util::maybe_submit_pr_with_env(
        repo_path,
        repo_for_pr.as_ref(),
        &util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
            branch_name: pr_branch,
            // Scoop publishes commit directly to the bucket branch;
            // the optional PR is informational. The winget/krew/cask
            // `update_existing_pr:` flag has no analogue on
            // `ScoopConfig` because there's no real "blocked queue" to
            // recover from here.
            update_existing_pr: false,
        },
        &format!("Update {} manifest to {}", manifest_name, version),
        &format!(
            "## Manifest\n- **Name**: {}\n- **Version**: {}\n\n{}",
            manifest_name,
            version,
            crate::util::SUBMITTED_BY_FOOTER
        ),
        "scoop",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
        ctx.env_source(),
    );

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }

    Ok(outcome.is_pushed())
}

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. See the homebrew publisher for the same
/// pattern.
pub(crate) type ScoopTarget = anodizer_core::publish_evidence::ScoopTargetSnapshot;

pub(crate) fn decode_scoop_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<ScoopTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Scoop(s) => s.scoop_targets.clone(),
        _ => Vec::new(),
    }
}

/// Collapse recorded bucket-push targets to a unique set keyed by
/// `(repo_url, branch)`. First entry seen wins. See homebrew's
/// `dedup_homebrew_targets` for the same-revert-twice hazard.
pub(crate) fn dedup_scoop_targets(targets: &[ScoopTarget]) -> Vec<ScoopTarget> {
    let mut seen: std::collections::BTreeSet<(String, Option<String>)> =
        std::collections::BTreeSet::new();
    let mut out: Vec<ScoopTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        let key = (t.repo_url.clone(), t.branch.clone());
        if seen.insert(key) {
            out.push(t.clone());
        }
    }
    out
}

pub(crate) fn collect_scoop_run_targets(ctx: &Context) -> Vec<ScoopTarget> {
    let mut out: Vec<ScoopTarget> = Vec::new();
    let selected = &ctx.options.selected_crates;
    for c in ctx.config.crate_universe() {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(sc) = c.publish.as_ref().and_then(|p| p.scoop.as_ref()) else {
            continue;
        };
        if let Some((owner, name)) = util::resolve_repo_owner_name(sc.repository.as_ref()) {
            // Mirror the publish path's branch resolution (including the
            // versioned PR-branch default) so the recorded rollback branch
            // matches the branch actually pushed.
            let manifest_raw = sc.name.as_deref().unwrap_or(&c.name);
            let manifest_name = ctx
                .render_template(manifest_raw)
                .unwrap_or_else(|_| manifest_raw.to_string());
            let version = util::crate_scoped_version(ctx, c);
            out.push(ScoopTarget {
                target: c.name.clone(),
                repo_url: format!("https://github.com/{}/{}.git", owner, name),
                branch: util::resolve_branch_or_versioned(
                    ctx,
                    sc.repository.as_ref(),
                    &manifest_name,
                    &version,
                ),
                token_env_var: Some("SCOOP_BUCKET_TOKEN".to_string()),
            });
        }
    }
    out
}
