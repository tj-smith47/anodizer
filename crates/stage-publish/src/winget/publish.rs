use super::*;

pub fn publish_to_winget(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    // Clone the winget config upfront so subsequent helpers do not borrow from
    // `ctx.config`; that frees the `&mut ctx` call site at the end of the
    // function (`ctx.record_publisher_outcome`).
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;
    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?
        .clone();

    // Resolve identity first so dry-run short-circuits BEFORE the full manifest
    // render (which requires short_description/license/installers): a dry-run
    // only reports the coordinates it would push, exactly as before.
    let Some(identity) = resolve_winget_identity(ctx, crate_name, &winget_cfg, log)? else {
        return Ok(());
    };

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit WinGet manifest for '{}' (pkg={}) to {}/{}",
            crate_name, identity.package_id, identity.repo_owner, identity.repo_name
        ));
        return Ok(());
    }

    // Reuse the identity already resolved above so the manifest render does not
    // re-run `resolve_winget_publisher_name` (which would re-emit its
    // fallback-to-repo-owner warning a second time per publish).
    let rendered =
        render_winget_manifests_with_identity(ctx, crate_name, &winget_cfg, &identity, log)?;

    submit_winget_manifests(ctx, log, &winget_cfg, &rendered)
}

/// Clone the package repo, write the pre-rendered manifests, commit, push, and
/// open a PR. The manifests are produced upstream by
/// [`render_winget_manifests_for_crate`] so this function performs only the
/// side-effecting steps.
pub(crate) fn submit_winget_manifests(
    ctx: &mut Context,
    log: &StageLogger,
    winget_cfg: &anodizer_core::config::WingetConfig,
    rendered: &RenderedWingetManifests,
) -> Result<()> {
    let version = ctx.version();
    let repo_owner = rendered.repo_owner.as_str();
    let repo_name = rendered.repo_name.as_str();
    let package_id = rendered.package_id.as_str();
    let ver_yaml = rendered.version_yaml.as_str();
    let inst_yaml = rendered.installer_yaml.as_str();
    let locale_yaml = rendered.locale_yaml.as_str();

    // Guard before the fork clone: a residual delimiter must bail with no
    // clone/commit/push side effect, not just no push.
    util::guard_no_unrendered(ctx, log, "winget version manifest", ver_yaml)?;
    util::guard_no_unrendered(ctx, log, "winget installer manifest", inst_yaml)?;
    util::guard_no_unrendered(ctx, log, "winget locale manifest", locale_yaml)?;

    let token = util::resolve_repo_token(
        ctx,
        winget_cfg.repository.as_ref(),
        Some("WINGET_PKGS_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("winget: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        ctx,
        winget_cfg.repository.as_ref(),
        repo_owner,
        repo_name,
        token.as_deref(),
        repo_path,
        "winget",
        log,
    )?;

    let manifest_dir = write_winget_manifests_to_disk(
        repo_path,
        package_id,
        &version,
        rendered.path.as_deref(),
        &rendered.default_locale,
        ver_yaml,
        inst_yaml,
        locale_yaml,
    )?;

    log.status(&format!(
        "wrote WinGet manifests to {}",
        manifest_dir.display()
    ));

    let commit_msg = render_winget_commit_msg(
        winget_cfg.commit_msg_template.as_deref(),
        package_id,
        &version,
        log,
        ctx.render_is_strict(),
    )?;

    let auto_branch = format!("{}-{}", package_id, version);
    let branch_name =
        util::resolve_branch(ctx, winget_cfg.repository.as_ref()).unwrap_or(auto_branch);
    let branch_name = branch_name.as_str();
    let commit_opts = util::resolve_commit_opts(ctx, winget_cfg.commit_author.as_ref(), log)?;
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch_name),
        "winget",
        &commit_opts,
        log,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "WinGet manifest pushed to {}/{} branch '{}'",
                repo_owner, repo_name, branch_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, winget manifest for '{}' already up to date",
                package_id
            ));
        }
    }

    let update_existing_pr = match winget_cfg.update_existing_pr.as_ref() {
        Some(v) => v
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("winget: render update_existing_pr condition")?,
        None => false,
    };

    let pr_outcome = submit_winget_pr(
        repo_path,
        winget_cfg.repository.as_ref(),
        repo_owner,
        repo_name,
        branch_name,
        package_id,
        &version,
        update_existing_pr,
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
        ctx.env_source(),
    );

    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WingetPublisher — Publisher trait wrapper (Submitter group)
// ---------------------------------------------------------------------------
//
// WinGet is structurally a Submitter publisher: each successful per-crate
// publish opens a PR against `microsoft/winget-pkgs` (or the upstream the
// `repository.pull_request.base` override names). That PR then goes
// through *automated validation* + *manual maintainer review*. Auto-closing
// a PR mid-validation is unreliable — the validation pipeline interacts
// with PR state in ways that can interfere with `gh pr close` — so unlike
// the krew publisher we do NOT close the PR programmatically on
// rollback. Instead, the rollback path warns per recorded target with
// the upstream coordinates and the operator's fork branch so a human
// can close the PR via the GitHub UI.
//
// CREDENTIAL HANDLING: [`WingetTarget`] stores no auth material. The
// GitHub token feeding the publish path (resolved through
// `repository.git.access_token` / `ANODIZER_GITHUB_TOKEN` /
// `GITHUB_TOKEN`) is irrelevant to a warn-only rollback — we only name
// the env var operators are expected to have set if they want to
// re-run publish, not the resolved value.

// Submitter-group `Publisher` for winget. Wraps the existing per-crate
// `publish_to_winget` entrypoint. Rollback is warn-only — winget PRs
// require manual operator action against `microsoft/winget-pkgs`
// (or the configured `repository.pull_request.base` upstream).
simple_publisher!(
    WingetPublisher,
    "winget",
    anodizer_core::PublisherGroup::Submitter,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. See the Submitter rustdoc above for the
/// credential-handling rationale.
pub(crate) type WingetTarget = anodizer_core::publish_evidence::WingetTargetSnapshot;

/// Decode the `winget_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
pub(crate) fn decode_winget_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<WingetTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Winget(w) => w.winget_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve the upstream `<owner>/<repo>` slug for a winget target —
/// mirrors the dispatch logic in `publish_to_winget`: prefer
/// `repository.pull_request.base` when set, else fall back to the
/// canonical `microsoft/winget-pkgs`.
///
/// Public for the same reason as [`static_package_identifier`]: `tag
/// rollback`'s published-state guard must search the same upstream the
/// publisher would submit to.
pub fn resolve_winget_upstream(
    winget_cfg: &anodizer_core::config::WingetConfig,
) -> (String, String) {
    if let Some(base) = winget_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.base.as_ref())
        && let (Some(o), Some(n)) = (base.owner.as_deref(), base.name.as_deref())
    {
        return (o.to_string(), n.to_string());
    }
    ("microsoft".to_string(), "winget-pkgs".to_string())
}

/// The crate-level `publish.winget` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::WingetConfig> {
    p.winget.as_ref()
}

pub(crate) fn is_winget_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Build a [`WingetTarget`] for the given crate. Reads config + the
/// live process version so the recorded coordinates match what
/// `publish_to_winget` will push. Returns `None` when no winget block
/// is configured or when the publisher / repo resolution would itself
/// no-op (matches the publish path's skip semantics).
pub(crate) fn collect_winget_target(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<WingetTarget>> {
    let Some(c) = crate::util::find_crate_in_universe(ctx, crate_name) else {
        return Ok(None);
    };
    let Some(cfg) = c.publish.as_ref().and_then(|p| p.winget.as_ref()) else {
        return Ok(None);
    };
    let Some((repo_owner, _repo_name)) =
        crate::util::resolve_repo_owner_name(cfg.repository.as_ref())
    else {
        return Ok(None);
    };
    let fork_owner = util::render_or_warn(ctx, log, "winget.repository.owner", &repo_owner)?;

    let name_raw = cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = util::render_or_warn(ctx, log, "winget.name", name_raw)?;

    let publisher_name = match cfg.publisher.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => fork_owner.clone(),
    };

    let auto_pkg_id = auto_package_identifier(&publisher_name, &name_rendered);
    let package_id = cfg
        .package_identifier
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or(auto_pkg_id);

    let version = ctx.version();
    let auto_branch = format!("{}-{}", package_id, version);
    let branch = crate::util::resolve_branch(ctx, cfg.repository.as_ref()).unwrap_or(auto_branch);

    let (upstream_owner, upstream_repo) = resolve_winget_upstream(cfg);

    Ok(Some(WingetTarget {
        target: package_id.clone(),
        crate_name: crate_name.to_string(),
        package_id,
        version,
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
    }))
}

/// Message emitted just before delegating to `publish_to_winget`.
/// Anchors the winget activity (manifest generation, fork clone, push,
/// PR submission) to a specific crate in the log so multi-crate
/// workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate winget publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_winget` on (not
/// the count of successful PRs — `publish_to_winget` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line, and the gh CLI submission helper logs its own success/warn).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished winget publish — {} configured crate(s) processed",
        processed
    )
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.winget` block at the config level) but the
/// run path processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a
/// `publish.winget` block — so a zero-processed run means `--crate` /
/// `--all` matrix selection was non-empty AND filtered every
/// winget-configured crate out. Operators must see this — otherwise the
/// publisher's `succeeded` status hides the fact that nothing was
/// pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "winget publisher registered but 0 of {} effective crate(s) had a winget \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.winget block is set.",
        selected_total
    )
}

/// Winget entries across the crate universe whose `skip_upload:`/`if:`
/// evaluates active right now AND whose crate is in scope for `--crate` /
/// `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names, so a selected-but-skipped
/// crate cannot masquerade as active via an out-of-scope sibling). Shared by
/// [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge.
pub(crate) fn active_winget_configs(ctx: &Context) -> Vec<&anodizer_core::config::WingetConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.winget.as_ref())
        .filter(|w| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                None,
                w.skip_upload.as_ref(),
                w.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for WingetPublisher {
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
        active_winget_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        active_winget_configs(ctx)
            .into_iter()
            .flat_map(|w| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    w.repository.as_ref(),
                    Some("WINGET_PKGS_TOKEN"),
                )
            })
            .collect()
    }

    fn advisory_requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Every winget publish lands as a PR against the upstream index;
        // `gh pr create` is the preferred transport with a full REST-API
        // fallback, so `gh` is a recommendation, never a gate failure.
        if active_winget_configs(ctx).is_empty() {
            return Vec::new();
        }
        vec![anodizer_core::EnvRequirement::Tool {
            name: "gh".to_string(),
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<WingetTarget> = Vec::new();
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_winget_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "winget",
            selected.len(),
        ));
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_winget_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("winget", crate_name),
                );
                continue;
            }
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest — AND the snapshot target's version/branch —
            // carry the crate's version, not the first crate's (workspace
            // per-crate independent-version mode). The target snapshot is taken
            // BEFORE the publish path runs (inside the same scope) so a
            // mid-publish failure still leaves the operator a manual PR-close
            // pointer whose recorded branch matches the one actually pushed.
            let target = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let target = collect_winget_target(ctx, crate_name, &log)?;
                    publish_to_winget(ctx, crate_name, &log)?;
                    Ok(target)
                },
            )?;
            if let Some(t) = target {
                targets.push(t);
            }
        }
        let processed = targets.len();
        if processed == 0 {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("winget");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://github.com/{}/{}/pulls?q=head%3A{}%3A{}",
                first.upstream_owner, first.upstream_repo, first.fork_owner, first.branch
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::Winget(
            anodizer_core::publish_evidence::WingetExtra {
                winget_targets: targets,
            },
        );
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_winget_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "winget",
                "submitted PR targets",
            ));
            return Ok(());
        }
        // WinGet PRs go through automated validation; auto-close
        // mid-validation is unreliable. Surface a warn per recorded
        // target with the fork-branch query so the operator can find
        // and close the PR manually.
        for t in &targets {
            log.warn(&format!(
                "manual winget PR closure required for '{}' version '{}'; \
                 visit https://github.com/{}/{}/pulls?q=is%3Apr+head%3A{}%3A{} \
                 and close the PR (winget validation cannot be reliably \
                 cancelled programmatically mid-flight)",
                t.package_id, t.version, t.upstream_owner, t.upstream_repo, t.fork_owner, t.branch
            ));
        }
        log.status(&format!(
            "{} winget PR(s) require manual closure",
            targets.len()
        ));
        Ok(())
    }

    /// Probe every active winget-pkgs fork for existence + push scope before
    /// any publisher runs: a missing fork or a token without PR scope fails
    /// before the moderation boundary, after sibling publishers may already
    /// have shipped. (A duplicate open PR is covered by the state-query checker.)
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "WINGET_PKGS_TOKEN",
            ctx.config
                .crate_universe()
                .into_iter()
                .filter_map(|c| c.publish.as_ref().and_then(|p| p.winget.as_ref())),
            |w| {
                // Winget has no `skip` field; gate on skip_upload + if only.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    w.skip_upload.as_ref(),
                    w.if_condition.as_deref(),
                )
            },
            |w| w.repository.as_ref(),
        ))
    }
}
