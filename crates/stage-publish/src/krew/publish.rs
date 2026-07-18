use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

use super::*;

pub fn publish_to_krew(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<KrewPublishOutcome> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "krew")?;

    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    // Honor `skip` first (template-aware), then `skip_upload`. `skip` lets
    // projects that aren't kubectl plugins keep a krew block in shared
    // config and turn it off without removing the surrounding
    // repository/short_description boilerplate.
    if let Some(d) = krew_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("krew: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "skipped krew config for '{}' — skip=true",
                crate_name
            ));
            return Ok(KrewPublishOutcome::skipped());
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        krew_cfg.if_condition.as_deref(),
        &format!("krew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped krew for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(KrewPublishOutcome::skipped());
    }
    if util::should_skip_upload(
        krew_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("krew for '{crate_name}'")),
    )? {
        return Ok(KrewPublishOutcome::skipped());
    }

    // Resolve repository owner/name from `repository:` (RepositoryConfig).
    // Repository fields are template-rendered.
    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("krew: no repository config for '{}'", crate_name))?;
    let repo_owner = util::render_or_warn(ctx, log, "krew.repository.owner", &repo_owner_raw)?;
    let repo_name = util::render_or_warn(ctx, log, "krew.repository.name", &repo_name_raw)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit Krew plugin manifest for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(KrewPublishOutcome::skipped());
    }

    let version = ctx.version();

    // Render the plugin manifest via the same path the schema validator uses.
    // The skip / `if:` / skip_upload gates were already evaluated above; the
    // renderer re-checks them (returning None) but on this path always yields
    // Some. All field resolution, the one-binary-per-archive check, the
    // artifact collection, and the manifest serialization live in the shared
    // renderer so the validated document is byte-for-byte what is published.
    let Some(manifest) = render_krew_manifest_for_crate(ctx, crate_name, log)? else {
        return Ok(KrewPublishOutcome::skipped());
    };
    util::guard_no_unrendered(ctx, log, "krew manifest", &manifest)?;

    // The plugin's GitHub coordinates and the resolved plugin name are reused
    // below (webhook provenance, branch name, PR title). Recomputed here
    // (cheap, side-effect-free) because the renderer consumed them internally;
    // `resolve_plugin_name` is idempotent, so the value matches the manifest's
    // `metadata.name` exactly.
    let plugin_github = _crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| (gh.owner.clone(), gh.name.clone()));
    let plugin_name_rendered = resolve_plugin_name(krew_cfg.name.as_deref(), crate_name, |t| {
        ctx.render_template(t)
    })?;
    let plugin_name = plugin_name_rendered.as_str();

    // Clone the krew-index fork, write the plugin manifest, commit, push.
    let token =
        util::resolve_repo_token(ctx, krew_cfg.repository.as_ref(), Some("KREW_INDEX_TOKEN"));

    // A plugin already in krew-index takes the self-contained webhook
    // flow: anodizer POSTs the rendered manifest + tag to the hosted bot,
    // which opens the version-bump PR server-side. A plugin not yet in
    // the index takes the PR-direct flow below (clone fork → write
    // manifest → open the initial PR). In `auto` the choice comes from a
    // token-authenticated membership probe that hard-errors on an
    // indeterminate result; `mode: bot` / `mode: pr-direct` force the
    // flow and skip the probe.
    let mode = krew_cfg.mode.unwrap_or_default();
    let flow = detect_krew_flow(mode, plugin_name, token.as_deref())?;
    if flow == KrewFlow::BotWebhook {
        // The bot identifies the submission by the plugin's OWN GitHub
        // repo (owner/repo/tag), not the krew-index fork coordinates
        // resolved above. Require it: the server records these in the
        // PR's provenance, so missing coordinates would silently
        // mis-target the bot.
        let (plugin_owner, plugin_repo) = plugin_github.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "krew: plugin '{}' is in krew-index (webhook flow) but has no \
                 `release.github` owner/repo — the krew-release-bot webhook \
                 needs the plugin's GitHub repo to identify the submission",
                plugin_name
            )
        })?;
        // Actor should be a GitHub login. Prefer the CI-provided
        // GITHUB_ACTOR, then ANODIZER_GITHUB_ACTOR, falling back to the
        // plugin repo owner. The owner is a best-effort fallback, not
        // guaranteed to be a personal login — an org-owned repo's owner is
        // the org slug, which is not a user account. The webhook only echoes
        // the actor into the PR's provenance text, so an org slug here is
        // cosmetic rather than a hard failure.
        let env = ctx.env_source();
        let actor = env
            .var("GITHUB_ACTOR")
            .or_else(|| env.var("ANODIZER_GITHUB_ACTOR"))
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| plugin_owner.clone());
        let webhook_url = resolve_webhook_url(env);
        let request = KrewReleaseRequest::new(
            &format!("v{}", version),
            plugin_name,
            &plugin_owner,
            &plugin_repo,
            &actor,
            &manifest,
        );
        submit_krew_release_webhook(&webhook_url, &request, plugin_name, &version, log)?;
        return Ok(KrewPublishOutcome { pushed: false });
    }
    log.status(&format!(
        "publishing krew plugin '{}' via pr-direct",
        plugin_name
    ));

    let tmp_dir = tempfile::tempdir().context("krew: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        ctx,
        krew_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "krew",
        log,
    )?;

    // Write plugin manifest under plugins/<name>.yaml.
    let plugins_dir = repo_path.join("plugins");
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("krew: create plugins dir {}", plugins_dir.display()))?;

    let manifest_file = plugins_dir.join(format!("{}.yaml", plugin_name));
    std::fs::write(&manifest_file, &manifest)
        .with_context(|| format!("krew: write manifest {}", manifest_file.display()))?;

    log.status(&format!(
        "wrote Krew plugin manifest {}",
        manifest_file.display()
    ));

    let commit_msg = crate::homebrew::render_commit_msg(
        krew_cfg.commit_msg_template.as_deref(),
        plugin_name,
        &version,
        "plugin",
        log,
        ctx.render_is_strict(),
    )?;
    let branch_name = format!("{}-v{}", plugin_name, version);
    let commit_opts = util::resolve_commit_opts(ctx, krew_cfg.commit_author.as_ref(), log)?;
    // Always create a versioned branch for Krew PRs.
    let branch = Some(branch_name.as_str());
    let push_outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        branch,
        "krew",
        &commit_opts,
        log,
    )?;
    let pushed = match push_outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Krew manifest pushed to {}/{} branch '{}'",
                repo_owner, repo_name, branch_name
            ));
            true
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, krew manifest for '{}' already up to date",
                plugin_name
            ));
            false
        }
    };

    // Submit a PR. When `repository.pull_request` is configured, use the
    // unified PR helper (which respects `base`, `draft`, `body`); otherwise
    // submit a PR via `gh` CLI against the canonical kubernetes-sigs/krew-index
    // (or `repository.pull_request.base` when set).
    let has_pr_config = krew_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    let update_existing_pr = match krew_cfg.update_existing_pr.as_ref() {
        Some(v) => v
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("krew: render update_existing_pr condition")?,
        None => false,
    };

    // Clone the repository config so the PR submission helpers no
    // longer borrow from `ctx.config` (via `krew_cfg`). NLL then
    // drops the immutable borrow, making the subsequent `&mut ctx`
    // call legal.
    let repo_for_pr = krew_cfg.repository.clone();

    let pr_outcome = if has_pr_config {
        util::maybe_submit_pr_with_env(
            repo_path,
            repo_for_pr.as_ref(),
            &util::PrOrigin {
                repo_owner: &repo_owner,
                repo_name: &repo_name,
                branch_name: &branch_name,
                update_existing_pr,
            },
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\n{}",
                crate_name,
                version,
                util::SUBMITTED_BY_FOOTER
            ),
            "krew",
            log,
            &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
            ctx.env_source(),
        )
    } else {
        // No `repository.pull_request:` block — always submit a PR against the
        // canonical kubernetes-sigs/krew-index slug (or the override in
        // `repository.pull_request.base`). Submitting against the user's own
        // fork would silently create useless intra-fork PRs against the user's
        // empty `main` branch instead of against the real upstream.
        let upstream_slug = repo_for_pr
            .as_ref()
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| match (base.owner.as_deref(), base.name.as_deref()) {
                (Some(o), Some(n)) => Some(format!("{}/{}", o, n)),
                _ => None,
            })
            .unwrap_or_else(|| "kubernetes-sigs/krew-index".to_string());

        util::submit_pr_via_gh_with_opts_with_env(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\n{}",
                crate_name,
                version,
                util::SUBMITTED_BY_FOOTER
            ),
            "krew",
            log,
            util::SubmitPrOpts { update_existing_pr },
            ctx.env_source(),
        )
    };

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(KrewPublishOutcome { pushed })
}
