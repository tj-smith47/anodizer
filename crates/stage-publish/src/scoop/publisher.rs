use super::*;

// ---------------------------------------------------------------------------
// ScoopPublisher — Publisher trait wrapper (git-revert rollback)
// ---------------------------------------------------------------------------

// Scoop bucket publisher. Mirrors the `homebrew` shape: each pushed
// manifest is recorded so a `--rollback-only` re-clones the bucket,
// runs `git revert HEAD --no-edit`, and pushes the revert. Scoop is
// always per-crate (no top-level Scoop config block), so the run loop
// only walks the per-crate universe (`Config::crate_universe`).
//
// CREDENTIAL HANDLING: ScoopTarget stores `token_env_var` — the NAME of
// the env var — not the resolved token VALUE. The token is read from the
// live env at rollback time so persisted evidence carries no secret
// material. Same rule applies to the homebrew / nix git-revert publishers.
simple_publisher!(
    ScoopPublisher,
    "scoop",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

/// The crate-level `publish.scoop` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::ScoopConfig> {
    p.scoop.as_ref()
}

pub(crate) fn is_scoop_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Message emitted just before delegating to `publish_to_scoop`. Anchors
/// the scoop activity (manifest render, bucket clone, push) to a specific
/// crate in the log so multi-crate workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate scoop publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_scoop` on (not the
/// count of successful bucket pushes — `publish_to_scoop` has its own
/// skip paths for skip_upload/dry-run/etc., each of which logs its own
/// status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished scoop publish — {} configured crate(s) processed",
        processed
    )
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_scoop_per_crate_configured`
/// check passed. `selected_len` is the size of the implicit-all-resolved
/// selection. The dry-run / skip_upload paths inside `publish_to_scoop`
/// return Ok(false) without pushing — `processed` must still increment
/// for them, otherwise this predicate fires a false-positive warning even
/// though the correct code path ran.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.scoop` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.scoop`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every scoop-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "scoop publisher registered but 0 of {} effective crate(s) had a scoop \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.scoop block is set.",
        selected_total
    )
}

/// Scoop entries across the crate universe whose `skip_upload:`/`if:`
/// evaluates active right now (scoop has no `skip` field) AND whose crate
/// is in scope for `--crate` / `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names, so a selected-but-skipped
/// crate cannot masquerade as active via an out-of-scope sibling). Shared by
/// [`anodizer_core::Publisher::requirements`],
/// [`anodizer_core::Publisher::preflight`], and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the active-entry
/// gate cannot diverge across the three call sites.
fn active_scoop_configs(ctx: &Context) -> Vec<&anodizer_core::config::ScoopConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.scoop.as_ref())
        .filter(|s| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                None,
                s.skip_upload.as_ref(),
                s.if_condition.as_deref(),
            )
        })
        .collect()
}

/// Build the open-PR reconcile target for `crate_name`, resolving the fork
/// repo, the upstream repo (`pull_request.base`, else the fork itself), and
/// the rendered manifest name — called inside `crate_name`'s own version
/// scope so the probed `version` matches what that crate would actually
/// publish under independent-version workspaces.
fn build_scoop_reconcile_target(
    ctx: &Context,
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
) -> anyhow::Result<Option<crate::util::PrReconcileTarget>> {
    let Some(scoop_cfg) = crate::util::find_crate_in_universe(ctx, crate_name)
        .and_then(|c| c.publish.as_ref())
        .and_then(|p| p.scoop.as_ref())
    else {
        return Ok(None);
    };
    let Some((fork_owner, fork_name)) =
        crate::util::resolve_repo_owner_name(scoop_cfg.repository.as_ref())
    else {
        return Ok(None);
    };
    let (upstream_owner, upstream_repo) = crate::util::resolve_upstream_coords(
        scoop_cfg.repository.as_ref(),
        &fork_owner,
        &fork_name,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );
    let manifest_name_raw = scoop_cfg.name.as_deref().unwrap_or(crate_name);
    let package = util::render_or_warn(ctx, log, "scoop.name", manifest_name_raw)?;
    let token = crate::util::resolve_repo_token(
        ctx,
        scoop_cfg.repository.as_ref(),
        Some("SCOOP_BUCKET_TOKEN"),
    );
    Ok(Some(crate::util::PrReconcileTarget {
        publisher: ScoopPublisher::PUBLISHER_NAME.into(),
        upstream_owner,
        upstream_repo,
        package,
        version: ctx.version(),
        token,
    }))
}

impl anodizer_core::Publisher for ScoopPublisher {
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_scoop_configs(ctx).is_empty()
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        active_scoop_configs(ctx)
            .into_iter()
            .flat_map(|s| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    s.repository.as_ref(),
                    Some("SCOOP_BUCKET_TOKEN"),
                )
            })
            .collect()
    }

    /// `Complete` = an OPEN upstream PR for this exact bucket manifest name +
    /// version already exists for EVERY active crate. Scoop's non-PR mode
    /// pushes a commit straight to the bucket branch — that push is
    /// idempotent (same content, same commit), so a PR-mode entry is the
    /// only shape this fast path can safely skip; a direct push always falls
    /// through to `run()`.
    fn reconcile(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::ReconcileState> {
        use anodizer_core::ReconcileState;
        if ctx.is_dry_run() {
            return Ok(ReconcileState::Absent);
        }
        let log = ctx.logger("publish");
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let selected = &ctx.options.selected_crates;
        let crate_names: Vec<String> = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
            .filter(|c| {
                c.publish
                    .as_ref()
                    .and_then(|p| p.scoop.as_ref())
                    .is_some_and(|s| {
                        !crate::publisher_helpers::entry_inactive(
                            ctx,
                            None,
                            s.skip_upload.as_ref(),
                            s.if_condition.as_deref(),
                        )
                    })
            })
            .map(|c| c.name.clone())
            .collect();
        if crate_names.is_empty() {
            return Ok(ReconcileState::Absent);
        }
        for crate_name in &crate_names {
            let Some(scoop_cfg) = crate::util::find_crate_in_universe(ctx, crate_name)
                .and_then(|c| c.publish.as_ref())
                .and_then(|p| p.scoop.as_ref())
            else {
                return Ok(ReconcileState::Absent);
            };
            if !crate::publisher_helpers::pull_request_enabled(scoop_cfg.repository.as_ref()) {
                return Ok(ReconcileState::Absent);
            }
        }
        let mut targets: Vec<crate::util::PrReconcileTarget> =
            Vec::with_capacity(crate_names.len());
        for crate_name in &crate_names {
            let target = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| build_scoop_reconcile_target(ctx, crate_name, &log),
            )?;
            match target {
                Some(t) => targets.push(t),
                None => return Ok(ReconcileState::Absent),
            }
        }
        Ok(crate::util::reconcile_open_prs(&targets, &policy, &log))
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_scoop_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "scoop",
            selected.len(),
        ));
        // `processed` counts crates whose configured predicate passed and
        // whose `publish_to_scoop` invocation was reached — NOT crates
        // that pushed. The dry-run / skip_upload paths inside
        // `publish_to_scoop` return Ok(false) without pushing; that's
        // still a successful run of the correct code path, so it must
        // not trigger the no-eligible-crates warning. `any_pushed` (below)
        // tracks the orthogonal "was a bucket mutated" question used
        // to gate evidence recording.
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_scoop_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("scoop", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest carries the crate's version, not the first
            // crate's (workspace per-crate independent-version mode).
            let pushed = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| publish_to_scoop(ctx, crate_name, &log),
            )?;
            if pushed {
                any_pushed = true;
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("scoop");
        if any_pushed {
            let targets = collect_scoop_run_targets(ctx);
            evidence.extra = anodizer_core::PublishEvidenceExtra::Scoop(
                anodizer_core::publish_evidence::ScoopExtra {
                    scoop_targets: targets,
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
        let targets = decode_scoop_targets(&evidence.extra);
        let unique = dedup_scoop_targets(&targets);
        util::run_token_revert_rollback(
            ctx,
            &unique,
            "scoop",
            "SCOOP_BUCKET_TOKEN",
            "bucket clone targets",
            "bucket",
        )
    }

    /// Probe every active bucket repo for existence + push scope before any
    /// publisher runs: a missing bucket or a token without push access fails
    /// the `git push` after sibling publishers may already have shipped.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "SCOOP_BUCKET_TOKEN",
            active_scoop_configs(ctx).into_iter(),
            |_s| true,
            |s| s.repository.as_ref(),
        ))
    }
}
