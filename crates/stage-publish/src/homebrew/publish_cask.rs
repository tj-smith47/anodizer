//! `publish_cask` — standalone cask publisher (used when the cask needs its
//! own tap repo, distinct from the formula tap).
use super::cask::{CaskGenResult, generate_cask_from_context};
use super::commit_msg::render_commit_msg;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// True when the standalone cask's effective skip gates trip for `crate_name`:
/// the cask `skip_upload` (per-cask wins, else the formula's `skip_upload`) is
/// truthy, or the effective `if:` (cask-level wins, else the formula's)
/// evaluates falsy. Logs the reason. Shared by the live `publish_cask` and the
/// validator-facing `render_homebrew_cask_for_crate` so the gate is defined
/// once.
fn cask_skip_gates_trip(
    ctx: &Context,
    hb_cfg: &anodizer_core::config::HomebrewConfig,
    cask_cfg: &anodizer_core::config::HomebrewCaskConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let effective_skip = cask_cfg
        .skip_upload
        .as_ref()
        .or(hb_cfg.skip_upload.as_ref());
    if crate::util::should_skip_upload(effective_skip, ctx, log)? {
        log.status(&format!(
            "skipped homebrew cask upload for '{}' — skip_upload={}",
            crate_name,
            effective_skip.map(|v| v.as_str()).unwrap_or("")
        ));
        return Ok(true);
    }

    // Cask-level `if:` wins; if unset, fall back to the parent formula's `if:`
    // so a per-crate gate on homebrew covers both surfaces in one declaration.
    let effective_if = cask_cfg
        .if_condition
        .as_deref()
        .or(hb_cfg.if_condition.as_deref());
    let proceed = anodizer_core::config::evaluate_if_condition(
        effective_if,
        &format!("homebrew cask publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped homebrew cask for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(true);
    }
    Ok(false)
}

/// Render the standalone Ruby cask a live publish would write for
/// `crate_name`, honoring the cask's effective `skip_upload` (per-cask wins,
/// else the formula's) and effective `if:` condition.
///
/// Returns `Ok(None)` when no cask is configured or the publisher would skip
/// it. The live publish path and the offline schema validator both render the
/// cask through the same [`generate_cask_from_context`] so the validated
/// document is byte-for-byte what a release pushes.
pub(crate) fn render_homebrew_cask_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<CaskGenResult>> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;
    let Some(hb_cfg) = publish.homebrew.as_ref() else {
        return Ok(None);
    };
    let Some(cask_cfg) = hb_cfg.cask.as_ref() else {
        return Ok(None);
    };

    if cask_skip_gates_trip(ctx, hb_cfg, cask_cfg, crate_name, log)? {
        return Ok(None);
    }

    let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg, log)?;
    Ok(Some(cask_result))
}

pub fn publish_cask(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;

    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew cask: no homebrew config for '{}'", crate_name))?;

    let cask_cfg = hb_cfg
        .cask
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew cask: no cask config for '{}'", crate_name))?;

    // Evaluate the cask's effective skip gates (per-cask wins, else the
    // formula's) before resolving the tap repo so a skipped cask with no
    // `repository:` block is a no-op rather than an error. The validator-facing
    // `render_homebrew_cask_for_crate` evaluates the same gates over the same
    // `generate_cask_from_context` render.
    if cask_skip_gates_trip(ctx, hb_cfg, cask_cfg, crate_name, log)? {
        return Ok(());
    }

    // Resolve repository owner/name from `repository:` (RepositoryConfig).
    let (repo_owner, repo_name) = crate::util::resolve_repo_owner_name(hb_cfg.repository.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!("homebrew cask: no repository config for '{}'", crate_name)
        })?;

    let version = ctx.version();
    let cask_name = cask_cfg.name.as_deref().unwrap_or(crate_name);

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Homebrew cask {}/{} for '{}'",
            repo_owner, repo_name, cask_name
        ));
        return Ok(());
    }

    let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg, log)?;

    // Clone tap repo, write cask, commit, push.
    let tmp_dir = tempfile::tempdir().context("homebrew cask: create temp dir")?;
    let repo_path = tmp_dir.path();

    let token = crate::util::resolve_repo_token(
        ctx,
        hb_cfg.repository.as_ref(),
        Some("HOMEBREW_TAP_TOKEN"),
    );
    crate::util::clone_repo(
        ctx,
        hb_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "homebrew cask",
        log,
    )?;

    // Honor `cask_cfg.directory:` so the tap can place casks in a sub-tree
    // (e.g. `Casks/versioned/`) instead of always landing under `Casks/`.
    // Directory defaults to "Casks".
    let directory = super::resolve_cask_directory(cask_cfg.directory.as_deref(), ctx)?;
    let casks_dir = repo_path.join(&directory);
    std::fs::create_dir_all(&casks_dir).with_context(|| {
        format!(
            "homebrew cask: create {} dir {}",
            directory,
            casks_dir.display()
        )
    })?;

    let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
    std::fs::write(&cask_path, &cask_result.content)
        .with_context(|| format!("homebrew cask: write cask file {}", cask_path.display()))?;
    log.status(&format!("wrote Homebrew cask {}", cask_path.display()));

    // `alternative_names:` versioned-file emission. Each entry that
    // renders to a token containing `@` (e.g. `myapp@1.2.3`) becomes its
    // own `.rb` file so `brew install myapp@1.2.3` installs a pinned
    // version. Aliases without `@` are rendered inline as `name "..."`
    // directives (handled in `generate_cask_from_context`).
    let mut written_paths: Vec<std::path::PathBuf> = vec![cask_path.clone()];
    for (alt_name, body) in &cask_result.versioned_files {
        let alt_path = casks_dir.join(format!("{}.rb", alt_name));
        std::fs::write(&alt_path, body).with_context(|| {
            format!(
                "homebrew cask: write versioned cask file {}",
                alt_path.display()
            )
        })?;
        log.status(&format!("wrote Homebrew cask {}", alt_path.display()));
        written_paths.push(alt_path);
    }

    let commit_msg = render_commit_msg(
        hb_cfg.commit_msg_template.as_deref(),
        &cask_result.cask_name,
        &version,
        "cask",
        log,
        ctx.render_is_strict(),
    )?;

    let path_strings: Vec<String> = written_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let path_refs: Vec<&str> = path_strings.iter().map(String::as_str).collect();
    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref(), log)?;
    let branch = crate::util::resolve_branch(ctx, hb_cfg.repository.as_ref());
    let outcome = crate::util::commit_and_push_with_opts(
        repo_path,
        &path_refs,
        &commit_msg,
        branch.as_deref(),
        "homebrew cask",
        &commit_opts,
    )?;
    match outcome {
        crate::util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Homebrew tap {}/{} updated with cask '{}'",
                repo_owner, repo_name, cask_result.cask_name
            ));
        }
        crate::util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push for homebrew cask — '{}' already up to date",
                cask_result.cask_name
            ));
        }
    }

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.as_deref().unwrap_or("main");
    let update_existing_pr = cask_cfg
        .update_existing_pr
        .as_ref()
        .map(|v| {
            v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    // Clone the repository config so the `maybe_submit_pr` call no
    // longer borrows from `ctx.config` (via `hb_cfg`). NLL then drops
    // the immutable borrow, making the subsequent `&mut ctx` call legal.
    let repo_for_pr = hb_cfg.repository.clone();

    let pr_outcome = crate::util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &crate::util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
            branch_name: pr_branch,
            update_existing_pr,
        },
        &format!("Update {} cask to {}", cask_result.cask_name, version),
        &format!(
            "## Cask\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
            cask_result.cask_name, version
        ),
        "homebrew cask",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(())
}
