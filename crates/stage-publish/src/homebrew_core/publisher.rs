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
//! Evidence: one `HomebrewCoreTargetSnapshot` per bumped formula — the
//! upstream, head owner, branch, and PR URL — so `anodizer tag rollback`
//! can find and close the open PR.

use anodizer_core::context::Context;

use super::api::GithubApi;
use super::formula::formula_is_current;
use super::locate::locate_formula;
use super::publish::publish_to_homebrew_core;
use super::resolve::{
    TOKEN_ENV_VARS, resolve_commit_message, resolve_download_url, resolve_formula_name,
    resolve_token, resolve_upstream,
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

/// Decode this publisher's targets back out of persisted evidence.
fn decode_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<HomebrewCoreTargetSnapshot> {
    match extra {
        anodizer_core::PublishEvidenceExtra::HomebrewCore(e) => e.homebrew_core_targets.clone(),
        _ => Vec::new(),
    }
}

/// Top-level `homebrew_cores:` entries whose `skip:`/`if:` evaluates active
/// right now. Shared by [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `preflight` keeps its own loop (it needs per-entry repository
/// resolution alongside the filter, not just a boolean).
fn active_homebrew_core_configs(ctx: &Context) -> Vec<&anodizer_core::config::HomebrewCoreConfig> {
    ctx.config
        .homebrew_cores
        .iter()
        .flatten()
        .filter(|entry| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            )
        })
        .collect()
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
    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_homebrew_core_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let mut out = Vec::new();
        for entry in active_homebrew_core_configs(ctx) {
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

    /// Open-PR reconcile: `Complete` when every active entry's formula
    /// already has an open bump PR upstream for this exact version.
    ///
    /// A homebrew-core bump is a PR against a moderated index, so an open PR
    /// IS the completed outcome — re-running would re-clone, re-render and
    /// re-push a branch for work that is already awaiting review. A merged or
    /// closed PR is `Absent` (a re-bump legitimately proceeds).
    fn reconcile(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::ReconcileState> {
        use anodizer_core::ReconcileState;
        if ctx.is_dry_run() {
            return Ok(ReconcileState::Absent);
        }
        let cfgs: Vec<anodizer_core::config::HomebrewCoreConfig> =
            active_homebrew_core_configs(ctx)
                .into_iter()
                .cloned()
                .collect();
        if cfgs.is_empty() {
            return Ok(ReconcileState::Absent);
        }
        let log = ctx.logger("publish");
        let deadline = ctx.retry_deadline();
        let version = ctx.version();
        let mut targets: Vec<crate::util::PrReconcileTarget> = Vec::new();
        for cfg in &cfgs {
            let Ok(formula) = resolve_formula_name(ctx, cfg) else {
                return Ok(ReconcileState::Absent);
            };
            let (upstream_owner, upstream_repo) = resolve_upstream(cfg);
            // An unresolvable token still probes: the GitHub search API serves
            // public repos unauthenticated, and homebrew-core is public.
            let token = resolve_token(ctx, cfg).ok().flatten().map(|t| t.token);
            // `resolve_commit_message` is templatable via
            // `commit_msg_template`, so re-deriving the default here would
            // silently miss any project that overrides it.
            let Ok(title) = resolve_commit_message(ctx, cfg, &formula, &version) else {
                return Ok(ReconcileState::Absent);
            };
            targets.push(crate::util::PrReconcileTarget {
                publisher: HomebrewCorePublisher::PUBLISHER_NAME.into(),
                title,
                upstream_owner,
                upstream_repo,
                package: formula,
                version: version.clone(),
                token,
            });
        }
        Ok(crate::util::reconcile_open_prs(
            &targets,
            &anodizer_core::retry::RetryPolicy::PREFLIGHT,
            deadline,
            &log,
        ))
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
            // A configured `repository.token` template that fails to render is
            // a real misconfiguration; surface it rather than treating it as
            // an absent token.
            let token = match resolve_token(ctx, cfg) {
                Ok(t) => t,
                Err(e) => {
                    acc = merge(acc, PreflightCheck::Warning(format!("{e:#}")));
                    continue;
                }
            };
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
            let token_value = token.as_ref().map(|t| t.token.as_str()).unwrap_or("");
            let Ok(api) = GithubApi::new(ctx.env_source(), token_value) else {
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
