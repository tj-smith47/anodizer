//! `publish_cask` — standalone cask publisher (used when the cask needs its
//! own tap repo, distinct from the formula tap).
use super::cask::generate_cask_from_context;
use super::commit_msg::render_commit_msg;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
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

    // GoReleaser Pro `homebrew_cask.if:` parity. Cask-level `if:` wins; if
    // unset, fall back to the parent formula's `if:` so a per-crate gate on
    // homebrew covers both surfaces in one declaration.
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
            "homebrew cask: skipping '{}' — `if` condition evaluated falsy",
            crate_name
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

    // Honor `cask_cfg.directory:` so the tap can place casks in a sub-tree
    // (e.g. `Casks/versioned/`) instead of always landing under `Casks/`.
    // Mirrors GR Pro `internal/pipe/cask/cask.go:65-67`.
    let directory_raw = cask_cfg.directory.as_deref().unwrap_or("Casks");
    let directory = ctx
        .render_template(directory_raw)
        .unwrap_or_else(|_| directory_raw.to_string());
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
    log.status(&format!("wrote Homebrew cask: {}", cask_path.display()));

    // GR Pro `alternative_names:` versioned-file emission. Each entry that
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
        log.status(&format!("wrote Homebrew cask: {}", alt_path.display()));
        written_paths.push(alt_path);
    }

    let commit_msg = render_commit_msg(
        hb_cfg.commit_msg_template.as_deref(),
        &cask_result.cask_name,
        &version,
        "cask",
    );

    let path_strings: Vec<String> = written_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let path_refs: Vec<&str> = path_strings.iter().map(String::as_str).collect();
    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref());
    let branch = crate::util::resolve_branch(hb_cfg.repository.as_ref());
    let outcome = crate::util::commit_and_push_with_opts(
        repo_path,
        &path_refs,
        &commit_msg,
        branch,
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
                "homebrew cask: nothing to push, cask '{}' already up to date",
                cask_result.cask_name
            ));
        }
    }

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
    );

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(())
}
