use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use crate::util;

use super::*;

// ---------------------------------------------------------------------------
// KrewPublisher — Publisher trait wrapper (close-PR rollback)
// ---------------------------------------------------------------------------

// Krew plugin-index publisher. Each successful per-crate publish opens a
// PR against an upstream `krew-index`-style repo from a fork. The rollback
// path closes those PRs via `PATCH /repos/<upstream>/pulls/<n>` with
// `state=closed`.
//
// PR-number discovery uses the query-at-rollback-time approach: at
// publish time only the upstream coordinates, the fork owner, and the
// branch name the publish path pushed to are recorded. At rollback time
// open PRs filtered by `head=<fork_owner>:<branch>` are listed and each
// match is closed. This sidesteps modifying the unchanged
// `publish_to_krew` body to surface the new PR number, and stays robust
// against a stale evidence file stitched in from an older run.
//
// CREDENTIAL HANDLING: `KrewPrTarget` stores `token_env_var` — the
// NAME of the env var to consult at rollback time — not the resolved
// token VALUE. Same rule applies to every PR-based publisher that
// touches GitHub auth.

simple_publisher!(
    KrewPublisher,
    "krew",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. One entry per crate whose publish path
/// successfully pushed a branch to its fork.
pub(super) type KrewPrTarget = anodizer_core::publish_evidence::KrewTargetSnapshot;

/// Decode the `krew_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
pub(super) fn decode_krew_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<KrewPrTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Krew(k) => k.krew_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve the upstream `<owner>/<repo>` slug for a krew target — mirrors
/// the dispatch logic in `publish_to_krew`: prefer
/// `repository.pull_request.base` when set, else fall back to the
/// canonical kubernetes-sigs/krew-index.
fn resolve_krew_upstream(krew_cfg: &anodizer_core::config::KrewConfig) -> (String, String) {
    if let Some(base) = krew_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.base.as_ref())
        && let (Some(o), Some(n)) = (base.owner.as_deref(), base.name.as_deref())
    {
        return (o.to_string(), n.to_string());
    }
    ("kubernetes-sigs".to_string(), "krew-index".to_string())
}

/// Build a [`KrewPrTarget`] for each crate the publisher would run on.
/// Reads config + the live process version so the branch name matches
/// what `publish_to_krew` will push.
/// Snapshot the rollback PR target for a single crate under the version
/// currently scoped on `ctx`.
///
/// MUST be called inside the per-crate version scope so the recorded branch
/// (`{plugin}-v{version}`) matches the branch [`publish_to_krew`] actually
/// pushed — in workspace per-crate independent-version mode the global
/// `ctx.version()` is the FIRST crate's version, which would record the wrong
/// branch and orphan this crate's PR from rollback.
pub(super) fn collect_krew_target(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<KrewPrTarget>> {
    let version = ctx.version();
    let Some(c) = crate::util::find_crate_in_universe(ctx, crate_name) else {
        return Ok(None);
    };
    let Some(krew_cfg) = c.publish.as_ref().and_then(|p| p.krew.as_ref()) else {
        return Ok(None);
    };
    let Some((fork_owner_raw, _)) =
        crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
    else {
        return Ok(None);
    };
    let fork_owner = util::render_or_warn(ctx, log, "krew.repository.owner", &fork_owner_raw)?;
    // Plugin-name override resolved through the same single-source helper
    // as `publish_to_krew` so the rollback-evidence branch name cannot
    // drift from the manifest `metadata.name` / file basename / webhook.
    let plugin_name = resolve_plugin_name(krew_cfg.name.as_deref(), &c.name, |t| {
        ctx.render_template(t)
    })?;
    let branch = format!("{}-v{}", plugin_name, version);
    let (upstream_owner, upstream_repo) = resolve_krew_upstream(krew_cfg);
    Ok(Some(KrewPrTarget {
        target: c.name.clone(),
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
        token_env_var: Some("KREW_INDEX_TOKEN".to_string()),
    }))
}

/// The crate-level `publish.krew` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::KrewConfig> {
    p.krew.as_ref()
}

pub(crate) fn is_krew_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Message emitted just before delegating to `publish_to_krew`. Anchors
/// the krew activity (plugin manifest render, fork clone, PR submission)
/// to a specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate krew publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_krew` on (not the
/// count of successful PRs — `publish_to_krew` has its own skip paths for
/// skip_upload/dry-run/etc., each of which logs its own status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished krew publish — {} configured crate(s) processed",
        processed
    )
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_krew_per_crate_configured`
/// check passed and whose `publish_to_krew` invocation was reached.
/// `selected_len` is the size of the implicit-all-resolved selection.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.krew` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.krew`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every krew-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "krew publisher registered but 0 of {} effective crate(s) had a krew \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.krew block is set.",
        selected_total
    )
}

/// Krew entries across the crate universe whose `skip:`/`skip_upload:`/
/// `if:` evaluates active right now AND whose crate is in scope for
/// `--crate` / `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names, so a selected-but-skipped
/// crate cannot masquerade as active via an out-of-scope sibling). Shared by
/// [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge.
fn active_krew_configs(ctx: &Context) -> Vec<&anodizer_core::config::KrewConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.krew.as_ref())
        .filter(|k| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                k.skip.as_ref(),
                k.skip_upload.as_ref(),
                k.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for KrewPublisher {
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
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_krew_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Both krew flows need a token (PR-direct for the clone+PR, bot/auto
        // for the index probe + webhook), and the PR-direct flow clones
        // with git. `git` is declared unconditionally because `auto` mode
        // can resolve to PR-direct at run time.
        active_krew_configs(ctx)
            .into_iter()
            .flat_map(|k| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    k.repository.as_ref(),
                    Some("KREW_INDEX_TOKEN"),
                )
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_krew_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "krew",
            selected.len(),
        ));
        let mut processed = 0usize;
        let mut any_pushed = false;
        let mut targets: Vec<KrewPrTarget> = Vec::new();
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_krew_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("krew", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest — AND the rollback PR branch — carry the
            // crate's version, not the first crate's (workspace per-crate
            // independent-version mode). The target snapshot is collected inside
            // the same scope so its recorded branch matches the one pushed.
            let (pushed, target) = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let outcome = publish_to_krew(ctx, crate_name, &log)?;
                    let target = if outcome.pushed {
                        collect_krew_target(ctx, crate_name, &log)?
                    } else {
                        None
                    };
                    Ok((outcome.pushed, target))
                },
            )?;
            if pushed {
                any_pushed = true;
            }
            if let Some(t) = target {
                targets.push(t);
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("krew");
        // Record rollback evidence only for the PrDirect flow, which
        // pushes a branch + opens a PR anodizer can later close. The
        // BotWebhook flow has no anodizer-owned PR (the krew-release-bot
        // server opens it), so there is nothing to roll back and no
        // evidence to record.
        if any_pushed {
            evidence.extra = anodizer_core::PublishEvidenceExtra::Krew(
                anodizer_core::publish_evidence::KrewExtra {
                    krew_targets: targets,
                },
            );
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_krew_targets(&evidence.extra);

        // Only the PrDirect flow records PR targets; the BotWebhook flow
        // records none (the krew-release-bot server owns the PR). Nothing
        // to roll back when there are no targets.
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "krew",
                "PR targets",
            ));
            return Ok(());
        }

        // Resolve token at rollback time — never persisted in evidence.
        // Falls back to ANODIZER_GITHUB_TOKEN then GITHUB_TOKEN, same as
        // every git-revert publisher.
        let env = ctx.env_source();
        let resolve_token = |t: &KrewPrTarget| -> Option<String> {
            util::resolve_rollback_token(env, t.token_env_var.as_deref())
        };

        // Fan out at PR granularity, not target granularity: a single
        // krew target can map to multiple open PRs if the publish path
        // pushed the same branch twice (idempotent re-publish). We dedup
        // PR numbers per (upstream, n) so we don't try to close the same
        // PR twice when two targets share the same fork branch.
        struct CloseJob {
            upstream_owner: String,
            upstream_repo: String,
            pr_number: u64,
            token: String,
            target_label: String,
        }
        let mut jobs: Vec<CloseJob> = Vec::new();
        let mut seen: std::collections::BTreeSet<(String, String, u64)> =
            std::collections::BTreeSet::new();
        for t in &targets {
            let Some(token) = resolve_token(t) else {
                log.warn(&format!(
                    "skipped rollback for {} — no krew token resolvable (env var ${} / \
                     {} all unset)",
                    t.target,
                    t.token_env_var.as_deref().unwrap_or("KREW_INDEX_TOKEN"),
                    anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / "),
                ));
                continue;
            };
            let env_hint_for_target = t.token_env_var.as_deref().unwrap_or("KREW_INDEX_TOKEN");
            let pr_numbers = match crate::util::find_open_pr_numbers_for_head(
                &t.upstream_owner,
                &t.upstream_repo,
                &t.fork_owner,
                &t.branch,
                Some(&token),
                env_hint_for_target,
            ) {
                Ok(v) => v,
                Err(e) => {
                    // Auth-failure / repo-not-found / transport problems
                    // surface as actionable warns naming the actual
                    // failure mode — not the misleading "no PR found,
                    // verify manually" that previously fired here.
                    log.warn(&format!(
                        "failed to query krew upstream {}/{} for open PRs ({}); \
                         {} — manual cleanup required",
                        t.upstream_owner, t.upstream_repo, t.target, e
                    ));
                    continue;
                }
            };
            if pr_numbers.is_empty() {
                log.warn(&format!(
                    "no open krew PRs found for head={}:{} against {}/{}; \
                     verify manually",
                    t.fork_owner, t.branch, t.upstream_owner, t.upstream_repo,
                ));
                continue;
            }
            for n in pr_numbers {
                let key = (t.upstream_owner.clone(), t.upstream_repo.clone(), n);
                if seen.insert(key) {
                    jobs.push(CloseJob {
                        upstream_owner: t.upstream_owner.clone(),
                        upstream_repo: t.upstream_repo.clone(),
                        pr_number: n,
                        token: token.clone(),
                        target_label: t.target.clone(),
                    });
                }
            }
        }

        let env_hint = targets
            .first()
            .and_then(|t| t.token_env_var.as_deref())
            .unwrap_or("KREW_INDEX_TOKEN");

        // Three-bucket count: (closed, already_closed, failed).
        // `already_closed` is a success bucket — 404 / 410 / 422 from
        // the PATCH means the desired end-state ("PR not open") is
        // already true (maintainer closed it, repo renamed, PR
        // deleted). Re-running --rollback-only after a partial
        // success must NOT surface those as failures.
        let counts = std::sync::Mutex::new((0usize, 0usize, 0usize));
        for chunk in jobs.chunks(crate::util::ROLLBACK_PARALLELISM) {
            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(chunk.len());
                for job in chunk {
                    let log = log.clone();
                    let counts = &counts;
                    handles.push(s.spawn(move || {
                        let pr_url = format!(
                            "https://github.com/{}/{}/pull/{}",
                            job.upstream_owner, job.upstream_repo, job.pr_number
                        );
                        log.status(&format!(
                            "closing krew PR {} ({})",
                            job.target_label, pr_url
                        ));
                        let outcome = crate::util::close_pr_via_api(
                            &job.upstream_owner,
                            &job.upstream_repo,
                            job.pr_number,
                            &job.token,
                        );
                        match outcome {
                            crate::util::CloseOutcome::Closed => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.0 += 1;
                            }
                            crate::util::CloseOutcome::AlreadyClosed => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.1 += 1;
                                log.status(&format!(
                                    "krew PR {} ({}) already closed/deleted upstream — \
                                     rollback noticed the existing state",
                                    job.target_label, pr_url
                                ));
                            }
                            crate::util::CloseOutcome::Failed(err) => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.2 += 1;
                                log.warn(&crate::publisher_helpers::rollback_failure_warning_msg(
                                    "krew",
                                    &job.target_label,
                                    &pr_url,
                                    &err,
                                    Some(env_hint),
                                ));
                            }
                        }
                    }));
                }
                for h in handles {
                    crate::util::join_or_warn(h, &log, "krew");
                }
            });
        }
        // `into_inner` consumes the Mutex; poison here means a worker
        // panicked. Counter state is still valid (3-tuple of usize) so
        // recover and emit the summary rather than abandon the operator.
        let (closed, already_closed, failed) = match counts.into_inner() {
            Ok(c) => c,
            Err(poisoned) => {
                log.warn("krew mutex poisoned by worker panic; reporting counters as-of poison");
                poisoned.into_inner()
            }
        };
        log.status(&format!(
            "krew rollback closed {}, already-closed {}, failed {}",
            closed, already_closed, failed
        ));
        Ok(())
    }

    /// Probe every active krew-index fork for existence + push scope before any
    /// publisher runs: a missing fork or a token that cannot open the PR fails
    /// the PR-direct flow after sibling publishers may already have shipped.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "KREW_INDEX_TOKEN",
            ctx.config
                .crate_universe()
                .into_iter()
                .filter_map(|c| c.publish.as_ref().and_then(|p| p.krew.as_ref())),
            |k| {
                // Krew carries a `skip` field, unlike scoop/homebrew/winget.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    k.skip.as_ref(),
                    k.skip_upload.as_ref(),
                    k.if_condition.as_deref(),
                )
            },
            |k| k.repository.as_ref(),
        ))
    }
}
