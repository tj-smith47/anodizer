//! `publish_cask` — standalone cask publisher (used when the cask needs its
//! own tap repo, distinct from the formula tap).
use super::cask::generate_cask_from_context;
use super::commit_msg::render_commit_msg;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
pub fn publish_cask(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;

    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew cask: no homebrew config for '{}'", crate_name))?;

    let cask_cfg = hb_cfg
        .cask
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew cask: no cask config for '{}'", crate_name))?;

    // Check skip_upload before doing any work. Per-crate cask skip_upload
    // takes precedence; falls back to the formula's skip_upload.
    let effective_skip = cask_cfg
        .skip_upload
        .as_ref()
        .or(hb_cfg.skip_upload.as_ref());
    if crate::util::should_skip_upload(effective_skip, ctx, log) {
        log.status(&format!(
            "homebrew cask: skipping upload for '{}' (skip_upload={})",
            crate_name,
            effective_skip.map(|v| v.as_str()).unwrap_or("")
        ));
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

    let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg)?;

    // Clone tap repo, write cask, commit, push.
    let tmp_dir = tempfile::tempdir().context("homebrew cask: create temp dir")?;
    let repo_path = tmp_dir.path();

    let token = crate::util::resolve_repo_token(
        ctx,
        hb_cfg.repository.as_ref(),
        Some("HOMEBREW_TAP_TOKEN"),
    );
    crate::util::clone_repo(
        hb_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "homebrew cask",
        log,
    )?;

    let casks_dir = repo_path.join("Casks");
    std::fs::create_dir_all(&casks_dir)
        .with_context(|| format!("homebrew cask: create Casks dir {}", casks_dir.display()))?;

    let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
    std::fs::write(&cask_path, &cask_result.content)
        .with_context(|| format!("homebrew cask: write cask file {}", cask_path.display()))?;

    log.status(&format!("wrote Homebrew cask: {}", cask_path.display()));

    let commit_msg = render_commit_msg(
        hb_cfg.commit_msg_template.as_deref(),
        &cask_result.cask_name,
        &version,
        "cask",
    );

    let cask_lossy = cask_path.to_string_lossy();
    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref());
    let branch = crate::util::resolve_branch(hb_cfg.repository.as_ref());
    crate::util::commit_and_push_with_opts(
        repo_path,
        &[&cask_lossy],
        &commit_msg,
        branch,
        "homebrew cask",
        &commit_opts,
    )?;

    log.status(&format!(
        "Homebrew tap {}/{} updated with cask '{}'",
        repo_owner, repo_name, cask_result.cask_name
    ));

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.unwrap_or("main");
    let update_existing_pr = cask_cfg
        .update_existing_pr
        .as_ref()
        .map(|v| {
            v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    crate::util::maybe_submit_pr(
        repo_path,
        hb_cfg.repository.as_ref(),
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
    );

    Ok(())
}
