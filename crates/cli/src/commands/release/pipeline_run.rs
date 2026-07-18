use super::*;

/// `--rollback-only` short-circuits the pipeline: load the prior run's
/// `report.json`, re-attempt rollback for every Succeeded / RollbackFailed
/// entry, persist the result to `rollback.json`, and return. No build /
/// publish / announce stages run in this mode.
///
/// The rollback-only branch bypasses `Pipeline::run` entirely, so it must
/// invoke `emit_summary` itself for `--summary-json=<path>` to land on disk.
/// The call wraps both the rollback dispatch result and the early-error
/// return so the summary fires regardless of how `rollback_only` resolved.
pub(crate) fn run_rollback_only(ctx: &mut Context) -> Result<()> {
    let outcome = (|| -> Result<()> {
        let run_id = ctx
            .options
            .from_run
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--rollback-only requires --from-run=<id>"))?;
        let updated_report = anodizer_stage_publish::rollback_only::run(ctx, &run_id)?;
        ctx.set_publish_report(updated_report);
        Ok(())
    })();
    anodizer_stage_announce::emit_summary(ctx);
    outcome
}

/// Run before-hooks once env AND git vars are populated. Respects
/// `--skip=before`. Skipped in
/// `--merge` / `--split` / `--publish-only` modes — CI already validates
/// the code before tagging, and hook compilation can dirty the working
/// tree.
/// Config-derived environment preflight for a release: every enabled
/// stage/publisher declares its tools / env vars / endpoints / key material,
/// and all failures are reported in one pass with ZERO mutations. Runs ahead
/// of the `before:` hooks and the failure-policy boundary, so a missing secret
/// surfaces as "fix and re-run" — never after minutes of hook work, and never
/// as a destructive rollback of a tag the run did not touch.
///
/// Snapshot / dry-run / `--split` skip it (no upstream side effects to guard).
/// `--publish-only` runs it (that mode's missing secrets used to surface
/// mid-publish); `--announce-only` checks announce requirements alone, since
/// announcers fire sequentially with real side effects.
pub(crate) fn run_release_env_preflight(
    ctx: &Context,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<()> {
    if !opts.no_preflight && !opts.dry_run && !opts.snapshot && !opts.split {
        let scope = if opts.announce_only {
            crate::commands::preflight::PreflightScope::AnnounceOnly
        } else if opts.publish_only {
            crate::commands::preflight::PreflightScope::PublishOnly
        } else {
            crate::commands::preflight::PreflightScope::Full
        };
        let report = crate::commands::preflight::run_env_preflight(ctx, scope, log);
        if !report.ok() {
            anyhow::bail!(
                "preflight: {} environment failure(s) across {} check(s); \
                 fix the issues above or re-run with --no-preflight to override",
                report.failures.len(),
                report.checks
            );
        }
    } else if opts.no_preflight {
        log.warn(
            "preflight skipped via --no-preflight; missing tools / secrets / key material \
             will surface mid-pipeline (no idempotent recovery)",
        );
    }
    Ok(())
}

pub(crate) fn run_before_hooks(
    ctx: &Context,
    config: &Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<()> {
    // `before:` hooks produce INPUTS for later stages: the build stage (run
    // under `--split`) and the archive / nfpm stages (run under `--merge`).
    // Split and merge are SEPARATE process invocations (distinct CI jobs /
    // shards with no shared filesystem), so each phase must regenerate the
    // inputs it needs — running here in split AND merge is correct, not a
    // double-run within one process. Only `--publish-only` / `--announce-only`
    // legitimately skip: they operate on already-produced artifacts (no build,
    // no archive), so hook-generated inputs no longer apply.
    if !opts.publish_only
        && !opts.announce_only
        && !ctx.should_skip("before")
        && let Some(before) = &config.before
        && let Some(ref hooks) = before.hooks
    {
        pipeline::run_hooks(
            hooks,
            "before",
            HookRunContext::new(opts.dry_run, log, Some(ctx.template_vars())),
        )?;
    }
    Ok(())
}

/// Render `--release-notes-tmpl` now that template vars are populated and
/// point `ctx.options.release_notes_path` at the rendered file.
///
/// Skipped in `--publish-only` mode: the preserved dist already contains a
/// `release-notes.md` written by the upstream determinism-harness run from
/// the SAME template vars. Re-rendering here from the current tree could
/// clobber it with a divergent version (e.g. if the local checkout has new
/// commits since the harness ran). The harness-written file is the
/// authoritative one for the preserved bytes.
pub(crate) fn render_release_notes_tmpl(
    ctx: &mut Context,
    config: &Config,
    opts: &ReleaseOpts,
    release_notes_path: Option<(PathBuf, String)>,
    log: &StageLogger,
) -> Result<()> {
    if !opts.publish_only
        && !opts.announce_only
        && let Some((tmpl_path, raw_content)) = release_notes_path
    {
        let rendered = template::render(&raw_content, ctx.template_vars()).with_context(|| {
            format!(
                "failed to render release notes template: {}",
                tmpl_path.display()
            )
        })?;
        let dist = &config.dist;
        std::fs::create_dir_all(dist).ok();
        let rendered_path = dist.join("release-notes.md");
        std::fs::write(&rendered_path, &rendered).with_context(|| {
            format!(
                "failed to write rendered release notes: {}",
                rendered_path.display()
            )
        })?;
        ctx.options.release_notes_path = Some(rendered_path);
        log.verbose("rendered release notes template");
    }
    Ok(())
}

/// Dirty repo gate: error out if the repo has uncommitted changes unless
/// running in snapshot, nightly, or dry-run mode.
pub(crate) fn enforce_dirty_repo_gate(ctx: &Context) -> Result<()> {
    if git::is_git_dirty() && !ctx.is_snapshot() && !ctx.is_nightly() && !ctx.is_dry_run() {
        let status = git::git_status_porcelain();
        anyhow::bail!(
            "git repository is dirty; use --snapshot to release from a dirty tree, or commit your changes first.\n\nDirty files:\n{}",
            status
        );
    }
    Ok(())
}

/// Apply nightly overrides after git vars are populated: render
/// `nightly.version_template` (default
/// `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"`), then override
/// `Version` / `RawVersion` / `Tag` / `IsNightly` / `ReleaseName` template
/// vars. SDE-aware so the harness's two from-clean rebuilds stay
/// byte-stable.
pub(crate) fn apply_nightly_template_vars(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    let nightly_cfg = config.nightly.as_ref();

    // `IsNightly` must be set first so `version_template`, `tag_name`,
    // and `name_template` can all branch on `{{ if .IsNightly }}…{{ end }}`
    // when rendered below.
    ctx.template_vars_mut().set_bool("IsNightly", true);

    // Default: `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"`
    // — bumps the patch component and embeds the commit SHA so two nightly
    // runs at different commits produce distinct, commit-immutable
    // versions. Users can override with `nightly.version_template` to
    // match their own conventions (e.g. embed `{{ .Date }}` for a
    // date-stamped scheme).
    let version_tmpl = nightly_cfg
        .and_then(|c| c.version_template.as_deref())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly");
    let nightly_version = template::render(version_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly version_template: {version_tmpl}"))?;
    let nightly_version = nightly_version.trim().to_string();
    if nightly_version.is_empty() {
        anyhow::bail!(
            "nightly version_template rendered to an empty string (template: {version_tmpl})"
        );
    }
    ctx.template_vars_mut().set("Version", &nightly_version);
    ctx.template_vars_mut().set("RawVersion", &nightly_version);

    // Nightly templates `tag_name` (alongside `name_template`).
    // Render after `Version` / `RawVersion` / `IsNightly` are populated so
    // `{{ .Version }}` etc. resolve to the nightly-overridden values rather
    // than the base semver. Default literal "nightly" stays template-safe.
    let tag_tmpl = nightly_cfg
        .and_then(|c| c.tag_name.as_deref())
        .unwrap_or("nightly");
    let nightly_tag = template::render(tag_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly tag_name: {tag_tmpl}"))?;
    // Trim before both the empty-check and the `Tag` set: a template
    // rendering to whitespace (e.g. `"  edge  "`) would otherwise pass
    // the gate AND store padded whitespace into `Tag`, which GitHub's
    // Releases API rejects.
    let nightly_tag = nightly_tag.trim().to_string();
    if nightly_tag.is_empty() {
        anyhow::bail!(
            "nightly tag_name rendered to an empty string (template: {tag_tmpl}). \
             An empty tag would be rejected by GitHub's Releases API."
        );
    }
    ctx.template_vars_mut().set("Tag", &nightly_tag);

    let name_tmpl = nightly_cfg
        .and_then(|c| c.name_template.as_deref())
        .unwrap_or("{{ ProjectName }}-nightly");
    let release_name = template::render(name_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly name_template: {name_tmpl}"))?;
    ctx.template_vars_mut().set("ReleaseName", &release_name);

    log.verbose(&format!(
        "nightly version={}, tag={}, name={}",
        nightly_version, nightly_tag, release_name
    ));
    Ok(())
}

/// Apply the snapshot version template (one is always applied).
/// Default: `"{{ Version }}-SNAPSHOT-{{ ShortCommit }}"` when no snapshot
/// config exists. `RawVersion` is intentionally preserved as the numeric
/// semver base.
pub(crate) fn apply_snapshot_template_vars(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    let snapshot_tmpl = config
        .snapshot
        .as_ref()
        .map(|s| s.version_template.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("{{ Version }}-SNAPSHOT-{{ ShortCommit }}");
    let rendered_name =
        template::render(snapshot_tmpl, ctx.template_vars()).with_context(|| {
            format!(
                "failed to render snapshot version_template: {}",
                snapshot_tmpl
            )
        })?;
    if rendered_name.trim().is_empty() {
        anyhow::bail!("empty snapshot name after rendering version_template");
    }
    ctx.template_vars_mut().set("Version", &rendered_name);
    ctx.template_vars_mut().set("ReleaseName", &rendered_name);
    log.verbose(&format!(
        "snapshot version={}, release_name={}",
        rendered_name, rendered_name
    ));
    Ok(())
}

/// Run the pre-flight publisher-state check. Returns `Ok(true)` when
/// `--preflight` (check-only) succeeded and the caller should exit
/// without continuing into the rest of the pipeline; `Ok(false)`
/// otherwise.
///
/// Walks each enabled one-way-door publisher (cargo, choco, winget, aur)
/// and bails early if the target version is already submitted / approved
/// / pending — saving an entire wasted release cycle. Skipped in snapshot
/// / dry-run / split modes (no upstream side-effects) and when `publish`
/// is already in `skip_stages`.
pub(crate) fn run_publisher_preflight(
    ctx: &mut Context,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<bool> {
    // Preflight probes publisher state ahead of a publish; an already-published
    // release has no pending one-way-door transitions to guard against.
    if opts.announce_only {
        log.status("skipped preflight — --announce-only does not publish");
        return Ok(false);
    }
    let should_run_preflight = should_run_preflight_auto(
        opts.no_preflight,
        opts.snapshot,
        opts.dry_run,
        opts.split,
        ctx.should_skip("publish"),
    );
    if !(opts.preflight || should_run_preflight) {
        return Ok(false);
    }

    let report = anodizer_stage_publish::preflight::run_preflight(ctx, log)?;
    if report.entries.is_empty() {
        log.verbose("skipped one-way-door preflight — no one-way-door publishers configured");
    } else {
        // Route the report through the stage logger (same channel as every
        // other status string in this function) instead of a raw `print!` so
        // verbosity / quiet flags / future redirection apply uniformly. The
        // Display impl is multi-line; splitting line-by-line preserves the
        // single-line cadence used by surrounding `log.status` calls.
        for line in report.to_string().trim_end_matches('\n').lines() {
            log.status(line);
        }
    }
    // Effective preflight strictness: the global `--strict` implies it, the
    // explicit `--strict-preflight` is kept for anyone who already plumbed it
    // through their CI, and the config-level `preflight.strict` turns it on
    // per-project. Beyond promoting Unknown publisher-state entries below, the
    // same predicate promotes indeterminate probe outcomes to blockers inside
    // `run_preflight` (via each publisher's probe mapping).
    let strict_preflight = ctx.preflight_is_strict();
    if report.has_blockers(strict_preflight) {
        let blockers = report.blockers(strict_preflight);
        let labels: Vec<String> = blockers
            .iter()
            .map(|b| format!("{} ({})", b.publisher, b.state.label()))
            .collect();
        anyhow::bail!(
            "preflight: {} publisher(s) blocked the release: {}. \
             Resolve upstream (await moderation / merge or close the PR / bump version) \
             or re-run with --no-preflight to override.",
            blockers.len(),
            labels.join(", ")
        );
    }
    // Resilience-extension blockers (rollback-scope checks +
    // `Publisher::preflight()` returns) live in their own channel; bail when
    // any is present so the operator sees the problem before the pipeline
    // starts.
    if !report.blockers.is_empty() {
        anyhow::bail!(
            "preflight: {} resilience blocker(s): {}",
            report.blockers.len(),
            report.blockers.join("; "),
        );
    }
    log.status(&format!(
        "preflight found {} publisher(s) clean",
        report.clean_count()
    ));
    // `--preflight` is a check-only mode: signal early-exit to the caller.
    if opts.preflight { Ok(true) } else { Ok(false) }
}

/// The end-of-pipeline layer of the shared required-failure exit gate
/// ([`anodizer_core::publish_report::gate_required_failures`]): the skip
/// set, the failure filter, the name list, and the recovery hint all live
/// in core, so this gate and the publish stage's in-stage bail cannot
/// drift. Only the what-completed-before-this-error sentence is
/// CLI-specific; every release-flow exit (full pipeline, publish-only,
/// split per-crate) routes through this one wrapper.
pub(crate) fn gate_required_failures(ctx: &Context) -> Result<()> {
    anodizer_core::publish_report::gate_required_failures(
        ctx,
        "The release pipeline ran to completion, so rollback / \
         announce-gating / summary all observed final state; this non-zero \
         exit ensures CI and shell callers see the failure.",
    )
}

/// Filter `config.publishers` down to the entries the operator-selection
/// filter (`--skip` / `--publishers`) leaves selected, mirroring the built-in
/// dispatch chokepoint so custom publishers are not a second-class escape
/// hatch from the allowlist. A nameless entry resolves to its index label
/// (`publisher[i]`), so a non-empty `--publishers` allowlist deselects it; an
/// empty allowlist (the main release job) deselects only what `--skip` names.
/// Deselected entries are recorded in the skip memento and logged so the run
/// summary counts them, never silently dropped.
pub(crate) fn select_custom_publishers(
    ctx: &Context,
    publishers: &[anodizer_core::config::PublisherConfig],
    log: &StageLogger,
) -> Vec<anodizer_core::config::PublisherConfig> {
    publishers
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            let name = p.name.clone().unwrap_or_else(|| format!("publisher[{i}]"));
            if ctx.publisher_deselected(&name) {
                let reason = ctx.deselected_reason(&name);
                ctx.skip_memento.remember("publisher", &name, &reason);
                log.status(&reason);
                false
            } else {
                true
            }
        })
        .map(|(_, p)| p.clone())
        .collect()
}

/// Post-pipeline tasks: metadata writing, publishers, after hooks.
pub(crate) fn run_post_pipeline(
    ctx: &mut Context,
    config: &Config,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    // One collapsible section so the post-pipeline work (metadata write,
    // custom publishers, milestone closure, `after:` hooks) renders as a
    // grouped stage in CI just like the in-loop stages, instead of raw
    // flush-left output trailing the last `::endgroup::`.
    let _section = log.group("finalize");
    // Retag to the section name so body lines inside `::group::finalize`
    // render `[finalize]` rather than the pipeline-level `[release]` tag.
    let log = &log.with_stage("finalize");

    // Print artifact size table if configured
    helpers::run_report_sizes(ctx, config, log);

    // Write metadata.json and artifacts.json (written
    // even in dry-run mode; applies metadata.mod_timestamp when set).
    helpers::write_metadata_and_artifacts(ctx, config, log)?;

    // Run custom publishers — honoring the operator-selection filter
    // (`--skip` / `--publishers`) exactly like the built-in publishers do at
    // the dispatch chokepoint. A custom publisher IS a publisher: under a
    // publisher-scoped run (e.g. the npm job's `--publishers npm`) it must
    // self-deselect, otherwise an unrelated entry (the `minio-mirror` that
    // wants AWS_* the github-hosted runner never carries) keeps firing and
    // fails the job. A nameless entry resolves to its index label, so a
    // non-empty allowlist deselects it; an empty allowlist (the main release
    // job) deselects only what `--skip` names, so the mirror still runs there.
    // Deselection is recorded in the skip memento so the run summary counts
    // it, never silent.
    if let Some(ref publishers) = config.publishers
        && !publishers.is_empty()
    {
        let selected = select_custom_publishers(ctx, publishers, log);
        if !selected.is_empty() {
            log.status("running custom publishers...");
            crate::commands::publisher::run_publishers(
                &selected,
                ctx.artifacts.all(),
                ctx.template_vars(),
                dry_run,
                log,
                ctx.options.parallelism,
                Some(&ctx.skip_memento),
            )?;
        }
    }

    // Close milestones (skipped on nightly — a
    // rolling nightly tag does not correspond to a milestone closure).
    if let Some(ref milestones) = config.milestones {
        if ctx.is_nightly() {
            log.status("milestone close skipped — nightly run (GoReleaser parity)");
        } else {
            milestones::close_milestones(milestones, ctx, dry_run, log)?;
        }
    }

    run_post_pipeline_after_hooks_only(ctx, config, dry_run, log)
}

/// Run only the user-defined `after:` hooks. Extracted so
/// `--announce-only` can fire them post-announce without re-running
/// custom publishers / milestones / metadata writes (which already
/// fired during the prior end-to-end run).
///
/// Canonical key is `after.hooks:`. The legacy
/// `after.post:` spelling is folded into `hooks:` at config-parse
/// time by `HooksConfig::merge_hook_aliases`, so this reader only
/// needs the canonical field.
///
/// Note on `--merge` interaction: `before:` hooks DO run on merge (see
/// `run_before_hooks`) because the merge phase runs the archive / nfpm
/// stages, which consume hook-generated inputs (e.g. a generated man page
/// the archive packages), and merge is a separate process invocation with
/// no shared state from the split shards.
/// `after:` hooks intentionally DO run on merge — the shard pipeline
/// (`build_split_pipeline`) only executes the build stage and never
/// reaches `run_post_pipeline`, so the merge step is the only point at
/// which the user's post-release notifications / cleanup hooks fire.
/// Skipping them here would mean they never run.
pub(super) fn run_post_pipeline_after_hooks_only(
    ctx: &Context,
    config: &Config,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if let Some(after) = &config.after
        && let Some(ref hooks) = after.hooks
    {
        pipeline::run_hooks(
            hooks,
            "after",
            HookRunContext::new(dry_run, log, Some(ctx.template_vars())),
        )?;
    }

    Ok(())
}
