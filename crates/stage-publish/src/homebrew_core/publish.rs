//! `publish_to_homebrew_core` — the bump loop.
//!
//! Per `homebrew_cores[]` entry: evaluate the skip gates, locate the existing
//! formula, rewrite its `url`/`sha256`/`version` (or `tag:`/`revision:`), and
//! land it as a direct commit or a fork-based pull request, accumulating a
//! rollback-evidence snapshot for each formula it touches.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::api::{GithubApi, PrOutcome, download_sha256};
use super::formula::{
    FormulaRewrite, flat_formula_path, formula_is_current, rewrite_formula, sharded_formula_path,
};
use super::locate::locate_formula;
use super::publisher::HomebrewCoreTargetSnapshot;
use super::resolve::{
    ResolvedToken, TOKEN_ENV_VARS, bump_branch, is_homebrew_core, resolve_commit_identity,
    resolve_commit_message, resolve_download_url, resolve_formula_name, resolve_token,
    resolve_upstream,
};

/// Top-level publish entrypoint. Iterates each `homebrew_cores[]` entry and
/// bumps its formula. `targets` is an out-param so a mid-loop error still
/// yields rollback evidence for the PRs that already opened.
pub(crate) fn publish_to_homebrew_core(
    ctx: &Context,
    log: &StageLogger,
    targets: &mut Vec<HomebrewCoreTargetSnapshot>,
) -> Result<()> {
    let entries = match ctx.config.homebrew_cores {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };
    for (idx, cfg) in entries.iter().enumerate() {
        let label = cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("homebrew_cores[{}]", idx));
        log.status(&format!("processing homebrew-core bump '{}'", label));

        // ---- Skip gates ----
        if let Some(skip) = cfg.skip.as_ref() {
            let off = skip
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("homebrew-core: render skip template")?;
            if off {
                log.status("skipped homebrew-core entry — skip evaluates true");
                continue;
            }
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("homebrew-core entry '{}'", label),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped homebrew-core entry — `if` condition evaluated falsy");
            continue;
        }

        let formula = resolve_formula_name(ctx, cfg)?;
        let version = ctx.version();
        let (up_owner, up_repo) = resolve_upstream(cfg);
        let download_url = resolve_download_url(ctx, cfg)?;
        let new_tag = ctx.template_vars().get("Tag").cloned();
        let new_revision = ctx.template_vars().get("FullCommit").cloned();
        let message = resolve_commit_message(ctx, cfg, &formula, &version)?;
        let branch = bump_branch(&formula, &version);

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would bump formula {} to {} in {}/{} (url {})",
                formula, version, up_owner, up_repo, download_url
            ));
            continue;
        }

        let Some(ResolvedToken { token, env_var }) = resolve_token(ctx, cfg)? else {
            bail!(
                "homebrew-core: a GitHub token is required to bump {}/{} (entry '{}'). \
                 Set ${} (or ${}, or {}), or `homebrew_cores[].repository.token`.",
                up_owner,
                up_repo,
                label,
                TOKEN_ENV_VARS[0],
                TOKEN_ENV_VARS[1],
                anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / "),
            );
        };
        let token_env_var = env_var;
        let api = GithubApi::new(ctx.env_source(), &token)?;

        // ---- Resolve base branch + commit path ----
        let core = is_homebrew_core(&up_owner, &up_repo);
        let cfg_branch = cfg
            .repository
            .as_ref()
            .and_then(|r| r.branch.clone())
            .filter(|b| !b.is_empty());
        // `repo_info` is fetched lazily: its `default_branch` is only needed
        // when no base branch is configured, and its `can_push` only off the
        // core path (core always forks + PRs). The dominant
        // Homebrew/homebrew-core bump with an explicit base branch therefore
        // skips the GET /repos entirely — one fewer call, and no spurious
        // failure from a repo read the bump never needed.
        let repo_info = if cfg_branch.is_none() || !core {
            Some(api.repo_info(&up_owner, &up_repo)?)
        } else {
            None
        };
        let can_push = repo_info.as_ref().is_some_and(|r| r.can_push);
        // cfg branch wins; else the fetched default branch (always present
        // when cfg_branch is None, since repo_info was fetched for exactly
        // that case); the literal is an unreachable-in-practice safe default.
        let base_branch = cfg_branch
            .or_else(|| repo_info.as_ref().map(|r| r.default_branch.clone()))
            .unwrap_or_else(|| "main".to_string());

        // ---- Locate + rewrite the formula ----
        let Some(file) =
            locate_formula(ctx, cfg, &api, &up_owner, &up_repo, &base_branch, &formula)?
        else {
            bail!(
                "homebrew-core: formula '{}' not found in {}/{} (tried {} and {}) — \
                 this publisher bumps an EXISTING formula; submit the initial \
                 formula by hand first",
                formula,
                up_owner,
                up_repo,
                sharded_formula_path(&formula),
                flat_formula_path(&formula),
            );
        };
        if formula_is_current(&file.content, &download_url, new_tag.as_deref(), &version) {
            log.status(&format!(
                "formula {} in {}/{} already at {} — skipping (idempotent)",
                formula, up_owner, up_repo, version
            ));
            continue;
        }

        // Detect the formula form STRUCTURALLY: a git-based formula's own
        // `url` stanza carries `tag:`/`revision:` (a substring scan for
        // "tag:" false-positives on a comment or resource block). Git
        // formulae carry no source sha256; only the archive form needs one.
        let git_form = super::formula::detect_git_form(&file.content);
        // A git-form url is a `.git` clone URL — a tarball url would corrupt
        // it, so the url stanza is rewritten ONLY when the user explicitly
        // set `download_url`. The archive form always rewrites the url.
        let user_set_download_url = cfg.download_url.as_deref().is_some_and(|u| !u.is_empty());
        let rewrite_url = !git_form || user_set_download_url;
        let sha256 = if git_form {
            None
        } else if let Some(raw) = cfg.sha256.as_deref().filter(|s| !s.is_empty()) {
            Some(
                ctx.render_template(raw)
                    .context("homebrew-core: render sha256 template")?,
            )
        } else {
            log.verbose(&format!(
                "downloading {} to compute the formula sha256",
                download_url
            ));
            Some(download_sha256(&download_url)?)
        };
        let (new_text, summary) = rewrite_formula(
            &file.content,
            &FormulaRewrite {
                url: rewrite_url.then(|| download_url.clone()),
                sha256,
                version: version.clone(),
                // tag:/revision: apply to the git form only; the structural
                // stanza scoping in rewrite_formula ignores them otherwise.
                tag: if git_form { new_tag.clone() } else { None },
                revision: if git_form { new_revision.clone() } else { None },
            },
        )?;
        log.verbose(&format!(
            "rewrote {} (url={} sha256={} version={} tag={} revision={} revision_removed={})",
            file.path,
            summary.url_rewritten,
            summary.sha256_rewritten,
            summary.version_rewritten,
            summary.tag_rewritten,
            summary.revision_rewritten,
            summary.revision_removed,
        ));

        // ---- Commit path ----
        let commit_identity = resolve_commit_identity(ctx, cfg, log)?;
        let identity_ref = commit_identity
            .as_ref()
            .map(|(n, e)| (n.as_str(), e.as_str()));
        let update_existing_pr = cfg
            .update_existing_pr
            .as_ref()
            .map(|s| s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl)))
            .transpose()
            .context("homebrew-core: render update_existing_pr template")?
            .unwrap_or(false);
        // `direct_commit` and `repository.pull_request.enabled: false` are
        // equivalent spellings of "commit straight to the base branch" (the
        // latter is the idiom shared with the tap/scoop/nix publishers). When
        // both are present and disagree, the explicit `direct_commit` value wins
        // — it is the specific knob on this axis; `pull_request.enabled` is
        // consulted only when `direct_commit` is unset, so an explicit
        // `direct_commit: false` can never be overridden into a silent direct
        // commit by a stale `enabled: false`.
        let pr_disabled = cfg
            .repository
            .as_ref()
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|p| p.enabled)
            == Some(false);
        let direct = match cfg.direct_commit.as_ref() {
            Some(s) => s
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("homebrew-core: render direct_commit template")?,
            None => pr_disabled,
        };

        if direct && !core {
            if !can_push {
                bail!(
                    "homebrew-core: `direct_commit: true` but the token cannot push \
                     to {}/{} — grant push access or drop direct_commit",
                    up_owner,
                    up_repo
                );
            }
            api.put_file(
                &up_owner,
                &up_repo,
                &file.path,
                &base_branch,
                &message,
                &new_text,
                &file.sha,
                identity_ref,
            )?;
            log.status(&format!(
                "bumped formula {} to {} — committed to {}/{}@{}",
                formula, version, up_owner, up_repo, base_branch
            ));
            push_target(
                targets,
                &formula,
                &version,
                &up_owner,
                &up_repo,
                "",
                "",
                true,
                None,
                token_env_var.clone(),
            );
            continue;
        }

        // Same-repo branch when the token can push (never for core itself,
        // which only takes fork PRs from automation); fork otherwise.
        let head_owner = if !core && can_push {
            up_owner.clone()
        } else {
            api.ensure_fork(&up_owner, &up_repo)?
        };
        // Idempotency: an open PR from this head already bumps this version.
        let existing = crate::util::find_open_pr_numbers_for_head_with_env(
            &up_owner,
            &up_repo,
            &head_owner,
            &branch,
            Some(&token),
            token_env_var.as_deref().unwrap_or(TOKEN_ENV_VARS[0]),
            ctx.env_source(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;
        if !existing.is_empty() {
            if !update_existing_pr {
                log.warn(&format!(
                    "open PR already bumps {} to {} in {}/{} (#{}) — skipping (set \
                     `update_existing_pr: true` to force-refresh the PR branch in place)",
                    formula, version, up_owner, up_repo, existing[0]
                ));
                continue;
            }
            // Force-reset the bump branch to the fresh base and re-commit the
            // rewritten formula so the OPEN PR carries this run's content
            // (a same-version re-cut) rather than a stale earlier attempt —
            // never opening a duplicate PR.
            let base_sha = api.branch_sha(&up_owner, &up_repo, &base_branch)?;
            api.create_or_reset_branch(&head_owner, &up_repo, &branch, &base_sha)?;
            api.put_file(
                &head_owner,
                &up_repo,
                &file.path,
                &branch,
                &message,
                &new_text,
                &file.sha,
                identity_ref,
            )?;
            log.status(&format!(
                "refreshed existing PR bumping {} to {} in {}/{} (#{}) — branch {} updated in place",
                formula, version, up_owner, up_repo, existing[0], branch
            ));
            push_target(
                targets,
                &formula,
                &version,
                &up_owner,
                &up_repo,
                &head_owner,
                &branch,
                false,
                Some(format!(
                    "https://github.com/{}/{}/pull/{}",
                    up_owner, up_repo, existing[0]
                )),
                token_env_var.clone(),
            );
            continue;
        }

        let base_sha = api.branch_sha(&up_owner, &up_repo, &base_branch)?;
        api.create_or_reset_branch(&head_owner, &up_repo, &branch, &base_sha)?;
        api.put_file(
            &head_owner,
            &up_repo,
            &file.path,
            &branch,
            &message,
            &new_text,
            &file.sha,
            identity_ref,
        )?;
        let head = if head_owner == up_owner {
            branch.clone()
        } else {
            format!("{}:{}", head_owner, branch)
        };
        let pr_cfg = cfg
            .repository
            .as_ref()
            .and_then(|r| r.pull_request.as_ref());
        let draft = pr_cfg.and_then(|p| p.draft).unwrap_or(false);
        let body = match pr_cfg
            .and_then(|p| p.body.as_deref())
            .filter(|b| !b.is_empty())
        {
            Some(raw) => ctx
                .render_template(raw)
                .context("homebrew-core: render pull_request.body template")?,
            None => format!(
                "Bump **{}** to **{}**.\n\nCreated with `brew bump-formula-pr` \
                 semantics (url + sha256 rewrite).\n\n{}",
                formula,
                version,
                crate::util::SUBMITTED_BY_FOOTER
            ),
        };
        let pr_url = match api.create_pr(
            &up_owner,
            &up_repo,
            &message,
            &body,
            &head,
            &base_branch,
            draft,
        )? {
            PrOutcome::Created(number, url) => {
                log.status(&format!(
                    "bumped formula {} to {} — opened {}/{}#{} ({})",
                    formula, version, up_owner, up_repo, number, url
                ));
                Some(url)
            }
            PrOutcome::AlreadyExists => {
                log.status(&format!(
                    "open PR already bumps {} to {} in {}/{} — skipping (idempotent)",
                    formula, version, up_owner, up_repo
                ));
                // Record the target anyway: a concurrent run opened the PR
                // between the idempotency probe and this create, and rollback
                // finds+closes it by head+branch — so it MUST be in evidence.
                push_target(
                    targets,
                    &formula,
                    &version,
                    &up_owner,
                    &up_repo,
                    &head_owner,
                    &branch,
                    false,
                    None,
                    token_env_var.clone(),
                );
                continue;
            }
        };
        push_target(
            targets,
            &formula,
            &version,
            &up_owner,
            &up_repo,
            &head_owner,
            &branch,
            false,
            pr_url,
            token_env_var.clone(),
        );
    }
    Ok(())
}

/// Push one bumped-formula target into the rollback-evidence accumulator.
/// A single constructor for both the direct-commit and fork+PR arms so a
/// field addition can never skew one arm's snapshot from the other's.
#[allow(clippy::too_many_arguments)]
fn push_target(
    targets: &mut Vec<HomebrewCoreTargetSnapshot>,
    formula: &str,
    version: &str,
    up_owner: &str,
    up_repo: &str,
    head_owner: &str,
    branch: &str,
    direct_commit: bool,
    pr_url: Option<String>,
    token_env_var: Option<String>,
) {
    targets.push(HomebrewCoreTargetSnapshot {
        formula: formula.to_string(),
        version: version.to_string(),
        upstream_owner: up_owner.to_string(),
        upstream_repo: up_repo.to_string(),
        head_owner: head_owner.to_string(),
        branch: branch.to_string(),
        direct_commit,
        pr_url,
        token_env_var,
    });
}
