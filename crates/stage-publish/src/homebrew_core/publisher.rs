//! `HomebrewCorePublisher` — Submitter-group `Publisher` impl that bumps an
//! existing formula in `Homebrew/homebrew-core` (or a formula repository
//! override) purely through the GitHub API and opens a pull request.
//!
//! Classification:
//! * **Group**: Submitter — the bump is a PR against a moderated upstream.
//! * **Required default**: `false` — a failed bump PR is recoverable by
//!   hand and must not abort the release.
//! * **Rollback scope**: PR close (`pull_request:write`). Rollback closes
//!   the PR(s) this run opened; a `direct_commit` bump is warn-only.
//!
//! Evidence: one [`HomebrewCoreTargetSnapshot`] per bumped formula — the
//! upstream, head owner, branch, and PR URL — so `--rollback-only
//! --from-run` can find and close the open PR.

use anodizer_core::config::HomebrewCoreConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::api::{GithubApi, PrOutcome, RepoFile, download_sha256};
use super::formula::{
    FormulaRewrite, flat_formula_path, formula_is_current, rewrite_formula, sharded_formula_path,
};

simple_publisher!(
    HomebrewCorePublisher,
    "homebrew-core",
    anodizer_core::PublisherGroup::Submitter,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields have no
/// slot to land in.
pub(crate) type HomebrewCoreTargetSnapshot =
    anodizer_core::publish_evidence::HomebrewCoreTargetSnapshot;

/// Env var fallback ladder for the bump token: the dedicated
/// `HOMEBREW_CORE_GITHUB_TOKEN`, then `COMMITTER_TOKEN` (the name
/// mislav/bump-homebrew-formula-action consumes, so a project migrating from
/// that action keeps its existing secret), then the standard GitHub ladder
/// (`ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN`).
pub(crate) const TOKEN_ENV_VARS: [&str; 2] = ["HOMEBREW_CORE_GITHUB_TOKEN", "COMMITTER_TOKEN"];

/// The default formula repository when `repository:` is unset.
const CORE_OWNER: &str = "Homebrew";
const CORE_REPO: &str = "homebrew-core";

/// Resolve the bump token: `repository.token` (templated) wins, then the
/// [`TOKEN_ENV_VARS`] ladder, then the standard GitHub token ladder.
pub(crate) fn resolve_token(ctx: &Context, cfg: &HomebrewCoreConfig) -> Option<String> {
    let non_empty = |s: String| if s.is_empty() { None } else { Some(s) };
    if let Some(tok) = cfg
        .repository
        .as_ref()
        .and_then(|r| r.token.as_deref())
        .filter(|t| !t.is_empty())
    {
        let rendered = ctx.render_template(tok).unwrap_or_else(|_| tok.to_string());
        if !rendered.is_empty() {
            return Some(rendered);
        }
    }
    TOKEN_ENV_VARS
        .iter()
        .find_map(|v| ctx.env_var(v).and_then(non_empty))
        .or_else(|| {
            anodizer_core::git::resolve_github_token_with_env(None, &|key| ctx.env_var(key))
        })
}

/// Resolve the formula name: `cfg.name` (templated), else the first
/// `ids:`-scoped crate name, else the primary crate name, else the project
/// name.
pub(crate) fn resolve_formula_name(ctx: &Context, cfg: &HomebrewCoreConfig) -> Result<String> {
    if let Some(raw) = cfg.name.as_deref().filter(|n| !n.is_empty()) {
        return ctx
            .render_template(raw)
            .context("homebrew-core: render name template");
    }
    if let Some(first) = cfg.ids.as_ref().and_then(|ids| ids.first()) {
        return Ok(first.clone());
    }
    Ok(ctx
        .config
        .primary_crate_name()
        .map(str::to_string)
        .unwrap_or_else(|| ctx.config.project_name.clone()))
}

/// Resolve the formula repository `(owner, name)` — the configured
/// `repository:` when both halves are set, else `Homebrew/homebrew-core`.
pub(crate) fn resolve_upstream(cfg: &HomebrewCoreConfig) -> (String, String) {
    crate::util::resolve_repo_owner_name(cfg.repository.as_ref())
        .unwrap_or_else(|| (CORE_OWNER.to_string(), CORE_REPO.to_string()))
}

/// True when the bump targets `Homebrew/homebrew-core` itself, which never
/// accepts direct pushes or same-repo bot branches — always fork + PR.
fn is_homebrew_core(owner: &str, repo: &str) -> bool {
    owner.eq_ignore_ascii_case(CORE_OWNER) && repo.eq_ignore_ascii_case(CORE_REPO)
}

/// Derive the source-repo `(owner, repo)` for the default tarball URL: the
/// `ids:`-scoped (else primary) crate's `release.github`, then the top-level
/// `release.github`, then the origin remote — the latter two via the
/// canonical [`resolve_github_slug`] (config override → remote, applied once),
/// so the repo identity is never re-parsed ad hoc.
fn source_repo_coords(ctx: &Context, cfg: &HomebrewCoreConfig) -> Option<(String, String)> {
    let universe = ctx.config.crate_universe();
    let scoped = cfg
        .ids
        .as_ref()
        .and_then(|ids| ids.first())
        .and_then(|id| universe.iter().find(|c| &c.name == id))
        .or_else(|| {
            ctx.config
                .primary_crate_name()
                .and_then(|n| universe.iter().find(|c| c.name == n))
        });
    let gh = scoped
        .and_then(|c| c.release.as_ref())
        .and_then(|r| r.github.as_ref())
        .or_else(|| ctx.config.release.as_ref().and_then(|r| r.github.as_ref()));
    // A configured `release.github` is the slug override; absent one, the
    // resolver derives once from the origin remote.
    let owner = gh.and_then(|g| ctx.render_template(&g.owner).ok());
    let name = gh.and_then(|g| ctx.render_template(&g.name).ok());
    anodizer_core::git::resolve_github_slug(owner.as_deref(), name.as_deref())
        .ok()
        .map(|s| (s.owner().to_string(), s.name().to_string()))
}

/// Resolve the templated download URL, defaulting to the GitHub source
/// tarball for the release tag.
pub(crate) fn resolve_download_url(ctx: &Context, cfg: &HomebrewCoreConfig) -> Result<String> {
    if let Some(raw) = cfg.download_url.as_deref().filter(|u| !u.is_empty()) {
        return ctx
            .render_template(raw)
            .context("homebrew-core: render download_url template");
    }
    let Some((owner, repo)) = source_repo_coords(ctx, cfg) else {
        bail!(
            "homebrew-core: cannot derive the default download URL — set \
             `download_url:`, a `release.github` repo, or run inside a git \
             checkout with a github.com remote"
        );
    };
    let tag = ctx
        .template_vars()
        .get("Tag")
        .cloned()
        .unwrap_or_else(|| format!("v{}", ctx.version()));
    Ok(format!(
        "https://github.com/{}/{}/archive/refs/tags/{}.tar.gz",
        owner, repo, tag
    ))
}

/// The bump branch name for one formula + version.
pub(crate) fn bump_branch(formula: &str, version: &str) -> String {
    format!("bump-{}-{}", formula, version)
}

/// The default commit message / PR title: `<formula> <version>` — the form
/// homebrew-core's CI expects for version bumps.
pub(crate) fn resolve_commit_message(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
    formula: &str,
    version: &str,
) -> Result<String> {
    match cfg.commit_msg_template.as_deref().filter(|t| !t.is_empty()) {
        Some(raw) => ctx
            .render_template(raw)
            .context("homebrew-core: render commit_msg_template"),
        None => Ok(format!("{} {}", formula, version)),
    }
}

/// Locate the formula file: the configured `path:` (templated), else the
/// sharded core layout, else the flat tap layout.
fn locate_formula(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
    api: &GithubApi,
    owner: &str,
    repo: &str,
    branch: &str,
    formula: &str,
) -> Result<Option<RepoFile>> {
    if let Some(raw) = cfg.path.as_deref().filter(|p| !p.is_empty()) {
        let path = ctx
            .render_template(raw)
            .context("homebrew-core: render path template")?;
        return api.get_file(owner, repo, &path, branch);
    }
    if let Some(f) = api.get_file(owner, repo, &sharded_formula_path(formula), branch)? {
        return Ok(Some(f));
    }
    api.get_file(owner, repo, &flat_formula_path(formula), branch)
}

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

        let Some(token) = resolve_token(ctx, cfg) else {
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
        let api = GithubApi::new(ctx.env_source(), &token)?;

        // ---- Resolve base branch + commit path ----
        let core = is_homebrew_core(&up_owner, &up_repo);
        let repo_info = api.repo_info(&up_owner, &up_repo)?;
        let base_branch = cfg
            .repository
            .as_ref()
            .and_then(|r| r.branch.clone())
            .filter(|b| !b.is_empty())
            .unwrap_or(repo_info.default_branch);

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

        // Git-based formulae (`url ..., tag:, revision:`) carry no source
        // sha256; only compute the digest for the archive form.
        let uses_tag_form = file.content.contains("tag:");
        let sha256 = if uses_tag_form {
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
                url: download_url.clone(),
                sha256,
                version: version.clone(),
                tag: new_tag.clone(),
                revision: new_revision.clone(),
            },
        )?;
        log.verbose(&format!(
            "rewrote {} (url={} sha256={} version={} tag={} revision={})",
            file.path,
            summary.url_rewritten,
            summary.sha256_rewritten,
            summary.version_rewritten,
            summary.tag_rewritten,
            summary.revision_rewritten,
        ));

        // ---- Commit path ----
        let direct = cfg
            .direct_commit
            .as_ref()
            .map(|s| s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl)))
            .transpose()
            .context("homebrew-core: render direct_commit template")?
            .unwrap_or(false);

        if direct && !core {
            if !repo_info.can_push {
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
            )?;
            log.status(&format!(
                "bumped formula {} to {} — committed to {}/{}@{}",
                formula, up_owner, version, up_repo, base_branch
            ));
            targets.push(HomebrewCoreTargetSnapshot {
                formula,
                version,
                upstream_owner: up_owner,
                upstream_repo: up_repo,
                head_owner: String::new(),
                branch: String::new(),
                direct_commit: true,
                pr_url: None,
                token_env_var: Some(TOKEN_ENV_VARS[0].to_string()),
            });
            continue;
        }

        // Same-repo branch when the token can push (never for core itself,
        // which only takes fork PRs from automation); fork otherwise.
        let head_owner = if !core && repo_info.can_push {
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
            TOKEN_ENV_VARS[0],
            ctx.env_source(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;
        if !existing.is_empty() {
            log.status(&format!(
                "open PR already bumps {} to {} in {}/{} (#{}) — skipping (idempotent)",
                formula, version, up_owner, up_repo, existing[0]
            ));
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
                continue;
            }
        };
        targets.push(HomebrewCoreTargetSnapshot {
            formula,
            version,
            upstream_owner: up_owner,
            upstream_repo: up_repo,
            head_owner,
            branch,
            direct_commit: false,
            pr_url,
            token_env_var: Some(TOKEN_ENV_VARS[0].to_string()),
        });
    }
    Ok(())
}

/// Decode this publisher's targets back out of persisted evidence.
fn decode_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<HomebrewCoreTargetSnapshot> {
    match extra {
        anodizer_core::PublishEvidenceExtra::HomebrewCore(e) => e.homebrew_core_targets.clone(),
        _ => Vec::new(),
    }
}

impl anodizer_core::Publisher for HomebrewCorePublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    /// `true` — homebrew-core is a moderated public index; a nightly bump
    /// PR per night is spam. Mirrors the tap-based homebrew publisher.
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    /// Per active entry: the bump token — a templated `repository.token`'s
    /// env refs when configured, else the any-of ladder
    /// (`HOMEBREW_CORE_GITHUB_TOKEN` / `COMMITTER_TOKEN` /
    /// `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN`).
    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let mut out = Vec::new();
        for entry in ctx.config.homebrew_cores.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            let cfg_token = entry
                .repository
                .as_ref()
                .and_then(|r| r.token.as_deref())
                .filter(|t| !t.is_empty());
            match cfg_token {
                Some(_) => out.extend(crate::publisher_helpers::secret_requirement(
                    cfg_token,
                    TOKEN_ENV_VARS[0],
                )),
                None => out.push(anodizer_core::EnvRequirement::EnvAnyOf {
                    vars: TOKEN_ENV_VARS
                        .iter()
                        .chain(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.iter())
                        .map(|s| s.to_string())
                        .collect(),
                }),
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // Accumulate every PR that opened BEFORE a mid-loop failure so the
        // evidence still names them for rollback. On Err the evidence is
        // built from the partial set, the Failed outcome is recorded, and
        // Ok(evidence) is returned — bubbling Err would make dispatch drop
        // the evidence and orphan the opened PRs from the run report.
        let mut targets: Vec<HomebrewCoreTargetSnapshot> = Vec::new();
        let publish_err = publish_to_homebrew_core(ctx, &log, &mut targets).err();

        let mut evidence = anodizer_core::PublishEvidence::new("homebrew-core");
        if let Some(first) = targets.iter().find(|t| t.pr_url.is_some()) {
            evidence.primary_ref = first.pr_url.clone();
        }
        if !targets.is_empty() {
            evidence.extra = anodizer_core::PublishEvidenceExtra::HomebrewCore(
                anodizer_core::publish_evidence::HomebrewCoreExtra {
                    homebrew_core_targets: targets,
                },
            );
        }
        if let Some(e) = publish_err {
            log.error(&format!("homebrew-core: publish failed: {e:#}"));
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Failed(format!("{e:#}")));
        }
        Ok(evidence)
    }

    /// Close every PR this run opened (find-by-head + PATCH close — the
    /// krew/schemastore rollback shape). `direct_commit` bumps have no PR;
    /// those are warn-only with the landed branch named.
    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "homebrew-core",
                "bump PRs",
            ));
            return Ok(());
        }
        let env = ctx.env_source();
        for t in &targets {
            if t.direct_commit {
                log.warn(&format!(
                    "homebrew-core rollback cannot undo the direct commit bumping \
                     '{}' to {} on {}/{} — revert the commit manually",
                    t.formula, t.version, t.upstream_owner, t.upstream_repo
                ));
                continue;
            }
            let env_hint = t.token_env_var.as_deref().unwrap_or(TOKEN_ENV_VARS[0]);
            let Some(token) = crate::util::resolve_rollback_token(env, t.token_env_var.as_deref())
            else {
                log.warn(&format!(
                    "skipped rollback for formula '{}' — no GitHub token resolvable \
                     (${} / {} all unset)",
                    t.formula,
                    env_hint,
                    anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / "),
                ));
                continue;
            };
            let pr_numbers = match crate::util::find_open_pr_numbers_for_head_with_env(
                &t.upstream_owner,
                &t.upstream_repo,
                &t.head_owner,
                &t.branch,
                Some(&token),
                env_hint,
                env,
            ) {
                Ok(v) => v,
                Err(e) => {
                    log.warn(&format!(
                        "failed to query {}/{} for open bump PRs ({}); manual cleanup \
                         required",
                        t.upstream_owner, t.upstream_repo, e
                    ));
                    continue;
                }
            };
            if pr_numbers.is_empty() {
                log.warn(&format!(
                    "no open PR found for {}:{} against {}/{} — nothing to close \
                     (already closed or merged)",
                    t.head_owner, t.branch, t.upstream_owner, t.upstream_repo
                ));
                continue;
            }
            for n in pr_numbers {
                match crate::util::close_pr_via_api_with_env(
                    &t.upstream_owner,
                    &t.upstream_repo,
                    n,
                    &token,
                    env,
                ) {
                    crate::util::CloseOutcome::Closed => {
                        log.status(&format!(
                            "closed bump PR {}/{}#{} for formula '{}'",
                            t.upstream_owner, t.upstream_repo, n, t.formula
                        ));
                    }
                    crate::util::CloseOutcome::AlreadyClosed => {
                        log.status(&format!(
                            "bump PR {}/{}#{} already closed",
                            t.upstream_owner, t.upstream_repo, n
                        ));
                    }
                    crate::util::CloseOutcome::Failed(msg) => {
                        log.warn(&format!(
                            "failed to close bump PR {}/{}#{}: {} — close it manually",
                            t.upstream_owner, t.upstream_repo, n, msg
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Live pre-publish gate. Per active entry, everything surfaces as a
    /// Warning (never a Blocker): a missing token, a formula that does not
    /// exist in the target repo, and a formula already at the new version
    /// (the run path skips it idempotently) are all operator signals, not
    /// hard stops — the publisher itself defaults to `required: false`.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::merge;
        use anodizer_core::PreflightCheck;

        let mut acc = PreflightCheck::Pass;
        for cfg in ctx.config.homebrew_cores.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            ) {
                continue;
            }
            let formula = match resolve_formula_name(ctx, cfg) {
                Ok(f) => f,
                Err(e) => {
                    acc = merge(acc, PreflightCheck::Warning(format!("{e:#}")));
                    continue;
                }
            };
            let (up_owner, up_repo) = resolve_upstream(cfg);
            let token = resolve_token(ctx, cfg);
            if token.is_none() {
                acc = merge(
                    acc,
                    PreflightCheck::Warning(format!(
                        "homebrew-core: no GitHub token resolvable for the '{}' bump \
                         — set ${} (or ${}, or {})",
                        formula,
                        TOKEN_ENV_VARS[0],
                        TOKEN_ENV_VARS[1],
                        anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / "),
                    )),
                );
            }
            let Ok(api) = GithubApi::new(ctx.env_source(), token.as_deref().unwrap_or("")) else {
                continue;
            };
            let base_branch = match cfg
                .repository
                .as_ref()
                .and_then(|r| r.branch.clone())
                .filter(|b| !b.is_empty())
            {
                Some(b) => b,
                None => match api.repo_info(&up_owner, &up_repo) {
                    Ok(info) => info.default_branch,
                    Err(e) => {
                        acc = merge(
                            acc,
                            PreflightCheck::Warning(format!(
                                "homebrew-core: cannot query {}/{}: {e:#}",
                                up_owner, up_repo
                            )),
                        );
                        continue;
                    }
                },
            };
            match locate_formula(ctx, cfg, &api, &up_owner, &up_repo, &base_branch, &formula) {
                Ok(Some(file)) => {
                    let version = ctx.version();
                    let url = resolve_download_url(ctx, cfg).unwrap_or_default();
                    let tag = ctx.template_vars().get("Tag").cloned();
                    if formula_is_current(&file.content, &url, tag.as_deref(), &version) {
                        acc = merge(
                            acc,
                            PreflightCheck::Warning(format!(
                                "homebrew-core: formula '{}' in {}/{} is already at {} — \
                                 the publish will skip idempotently",
                                formula, up_owner, up_repo, version
                            )),
                        );
                    }
                }
                Ok(None) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "homebrew-core: formula '{}' not found in {}/{} — this \
                             publisher bumps an EXISTING formula",
                            formula, up_owner, up_repo
                        )),
                    );
                }
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "homebrew-core: could not probe formula '{}' in {}/{}: {e:#}",
                            formula, up_owner, up_repo
                        )),
                    );
                }
            }
        }
        Ok(acc)
    }
}
