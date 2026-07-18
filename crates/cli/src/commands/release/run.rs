use super::*;

pub fn run(mut opts: ReleaseOpts) -> Result<()> {
    if opts.prepare {
        apply_prepare_mode_to_skip(&mut opts.skip);
    }
    validate_strict_vs_allowlist(&opts)?;

    let log = StageLogger::new(
        "release",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    git::check_git_available()?;

    if opts.snapshot && opts.nightly {
        anyhow::bail!("--snapshot and --nightly cannot be combined");
    }

    let config_path =
        pipeline::find_config_with_logger(opts.config_override.as_deref(), Some(&log))?;
    // Load WITHOUT emitting advisories here: the submitter moderation-queue
    // advisories are deselection-aware and `Context` (which carries the
    // `--skip` / `--publishers` surface) does not exist yet. They are emitted
    // once below via `emit_config_advisories_filtered` after `ctx` is built,
    // so an advisory for a publisher the run deselected (e.g. chocolatey under
    // `--publishers npm`) is suppressed rather than printed as noise.
    let mut config = pipeline::load_config(&config_path)?;

    let workspace_skip = apply_workspace_overlay_for_opts(&mut config, &opts, &log)?;

    helpers::infer_project_name(&mut config, &log);
    helpers::auto_detect_github(&mut config, &log);

    apply_release_meta_overrides(&mut config, &opts)?;

    let all_known_crates: Vec<CrateConfig> = config.crate_universe().into_iter().cloned().collect();
    let selected_sorted = resolve_selected_crates(&opts, &all_known_crates, &config, &log)?;

    // Tags-at-HEAD default path: when no --crate and no --all were given and
    // HEAD has no matching tags, this is a no-op (the push that triggered this
    // run didn't include any release tags).
    //
    // Excluded modes: --snapshot / --nightly / --dry-run build without a real
    // tag; --publish-only / --announce-only / --rollback-only consume a prior
    // dist tree; --split / --merge drive a multi-host flow. All of those modes
    // use "empty selected_crates = all crates" and must not be short-circuited.
    if selected_sorted.is_empty()
        && opts.crate_names.is_empty()
        && !opts.all
        && !opts.snapshot
        && !opts.nightly
        && !opts.dry_run
        && !opts.publish_only
        && !opts.announce_only
        && !opts.rollback_only
        && !opts.split
        && !opts.merge
        // `--preflight-secrets` is a PRE-tag gate: by design it runs before
        // any tag exists at HEAD, so the tags-at-HEAD short-circuit must not
        // pre-empt it (it would otherwise exit 0 without checking secrets).
        && !opts.preflight_secrets
    {
        log.status("no release tags at HEAD — nothing to do");
        return Ok(());
    }

    // --host-targets: resolve the configured target union for the selected
    // crates down to the host-buildable subset, then feed it through the same
    // `targets` intersection-filter the Determinism Harness uses. Done here
    // (after crate selection, before the build context is assembled) so the
    // partition reflects the same per-crate build.targets / defaults.targets /
    // builds.ignore resolution every config mode shares.
    if opts.host_targets {
        resolve_host_targets(&mut opts, &config, &selected_sorted, &log)?;
    }

    let skip_stages = compute_skip_stages(opts.skip.clone(), &workspace_skip, opts.snapshot);

    let release_notes_path = read_release_notes_template(&opts)?;
    let rollback_mode = parse_rollback_mode(opts.rollback.as_deref())?;
    let simulate_failure_publishers = resolve_simulate_failure(&mut opts.simulate_failure)?;
    let runtime_nondeterministic_allowlist =
        parse_allow_nondeterministic(&opts.allow_nondeterministic)?;

    // Group the pre-pipeline setup (config-root resolution, env, git
    // context, `before:` hooks, snapshot/nightly notes, deprecation
    // warnings, milestone preflight) into one section so it renders as a
    // collapsible stage in CI rather than ungrouped flush-left output ahead
    // of the first `::group::`. Opened BEFORE `resolve_project_root` so its
    // bare-filename fallback warnings land inside the section too. The scope
    // block drops the guard before the mode dispatch below, so each mode
    // opens its own sections cleanly.
    let project_root;
    let mut ctx;
    {
        let _setup = log.group("setup");
        // Retag to the section name so body lines inside `::group::setup`
        // render `[setup]` rather than the pipeline-level `[release]` tag.
        let log = log.with_stage("setup");

        project_root = resolve_project_root(&config_path, Some(&log));

        let ctx_opts = build_context_options(
            &opts,
            skip_stages,
            selected_sorted,
            rollback_mode,
            simulate_failure_publishers,
            runtime_nondeterministic_allowlist,
            project_root,
        );
        ctx = Context::new(config.clone(), ctx_opts);
        // Install the pre-submitter verify-release gate once, at the single
        // production `Context::new` call site for the whole release
        // pipeline. `publish_only::run` / `run_per_crate` and the full
        // release path all dispatch from this same `ctx`, so installing
        // here covers every config mode (single-crate, lockstep, per-crate)
        // and both `release` / `release --publish-only` entry points for
        // free — no separate install call needed at either path. The
        // closure itself no-ops (`Ok(true)`) when `verify_release` is
        // disabled or out of scope, so this is unconditional rather than a
        // new config knob.
        install_verify_gate(&mut ctx);
        // Surface the submitter moderation-queue advisories now that `ctx`
        // carries the resolved `--skip` / `--publishers` selection surface,
        // skipping any whose publisher this run deselected (a `--publishers npm`
        // run must not print chocolatey/winget advisories). Verbose-only, once
        // per command — matching `load_config_logged`'s register/cardinality.
        pipeline::emit_config_advisories_filtered(&config, &log, |name| {
            ctx.publisher_deselected(name)
        });
        helpers::resolve_scm_token_type(&mut ctx, &config);
        ctx.populate_time_vars();
        ctx.populate_runtime_vars();
        ctx.populate_metadata_var()?;

        // Set explicitly in both branches so `{% if IsPrepare %}` evaluates
        // correctly either way (a missing var would short-circuit the
        // truthy arm even when prepare mode is requested).
        ctx.template_vars_mut().set_bool("IsPrepare", opts.prepare);

        // --rollback-only consumes a prior run's recorded state and never
        // builds; short-circuit before the env / git / hooks setup work
        // below (which it does not need). Returns from inside the setup
        // group — the guard drops on the early return, balancing the
        // section — so rollback's own output is not nested under later
        // setup steps it skips.
        if ctx.options.rollback_only {
            return run_rollback_only(&mut ctx);
        }

        // Dist-state enforcement (`--clean` removal / non-empty hard error)
        // emits its user-facing `would clean` note here so it sits inside
        // the setup section rather than ungrouped ahead of the run.
        //
        // Sequenced AFTER the tags-at-HEAD no-op short-circuit above on
        // purpose: a no-op run (push carried no release tags) must NOT wipe
        // a populated dist that a later --publish-only run will consume.
        // A real --clean release is never a no-op (it has selected crates,
        // or is --snapshot/--all/etc.), so it still falls through here and
        // cleans dist before the build stage in the pipeline below.
        enforce_dist_state(&config, &opts, &log)?;
        helpers::setup_env(&mut ctx, &config, &log)?;
        helpers::resolve_git_context(&mut ctx, &config, &log)?;

        // `--preflight-secrets`: a central pre-tag gate for decoupled CI
        // runners (build / determinism shards on many hosts plus a publish
        // runner) that all carry the SAME injected secrets but different
        // host-local tools. Validate every runner-agnostic credential across
        // the full release surface — env vars and env-borne key material,
        // dropping tools / docker daemon / endpoints / on-disk key files — and
        // exit with zero mutations. Placed AFTER env / git context resolution
        // (so `{{ .Env.* }}` refs render) but BEFORE `before:` hooks, the
        // dirty-tree gate, the publisher-state probe, and mode dispatch, so
        // the gate runs no hook, makes no network call, and starts no
        // pipeline. Returns from inside the setup group — the guard drops on
        // the early return, balancing the section.
        if ctx.options.preflight_secrets {
            let report = crate::commands::preflight::run_env_preflight(
                &ctx,
                crate::commands::preflight::PreflightScope::SecretsOnly,
                &log,
            );
            if !report.ok() {
                anyhow::bail!(
                    "preflight-secrets: {} secret/credential failure(s) across {} check(s); \
                     set the missing secrets above before tagging the release",
                    report.failures.len(),
                    report.checks
                );
            }
            log.status("preflight-secrets: all required publish secrets / credentials present");
            return Ok(());
        }

        // Config-derived environment preflight runs BEFORE the `before:` hooks
        // (which can take minutes): a missing secret / tool / key must abort
        // with zero mutations and zero wasted hook time, never after a long
        // prep. It probes declared tool/secret/endpoint *presence*, which is
        // version-independent; on --nightly it sees the base (pre-nightly)
        // version vars applied below, so gate nightly-only requirements on the
        // IsNightly bool rather than a rendered version string.
        run_release_env_preflight(&ctx, &opts, &log)?;

        run_before_hooks(&ctx, &config, &opts, &log)?;
        render_release_notes_tmpl(&mut ctx, &config, &opts, release_notes_path, &log)?;
        enforce_dirty_repo_gate(&ctx)?;

        if ctx.is_nightly() {
            apply_nightly_template_vars(&mut ctx, &config, &log)?;
        }
        if ctx.is_snapshot() {
            apply_snapshot_template_vars(&mut ctx, &config, &log)?;
        }

        // In publish-only the preserved dist/config.yaml is already on disk and
        // its sha256 was recorded at determinism-check time; re-rendering it from
        // the current binary's config serialization would diverge from that hash
        // and trip hash_verify_preserved_dist. The --split BUILD leg uses
        // opts.split (it legitimately generates the dist) so it still writes here.
        if !opts.publish_only {
            helpers::write_effective_config(&config, &log)?;
        }

        if !opts.split
            && !opts.announce_only
            && let Some(ref milestones) = config.milestones
        {
            milestones::preflight_milestones(milestones, &mut ctx, &log)?;
        }
    }

    if run_publisher_preflight(&mut ctx, &opts, &log)? {
        return Ok(());
    }

    // Every mode below routes its outcome through the in-process failure
    // policy (`release.on_failure`): on a pipeline failure the binary
    // itself decides rollback vs hold instead of leaving a summary for a
    // workflow-side `if:` chain to act on.
    let result = dispatch_release_modes(&mut ctx, &config, &opts, &log);
    failure_policy::finish(&ctx, &opts, &log, result)
}

/// Run the selected release mode (publish-only / announce-only / split /
/// merge / full pipeline). Split out of [`run`] so the caller can route
/// every mode's failure through [`failure_policy::finish`] uniformly,
/// while the zero-mutation preflight gates stay outside that boundary.
pub(crate) fn dispatch_release_modes(
    ctx: &mut Context,
    config: &Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<()> {
    if opts.publish_only {
        // --publish-only consumes the preserved dist tree (artifacts.json /
        // context.json) rather than git tags-at-HEAD. Crate selection comes
        // from what the harness built (recorded in <dist>/context.json), not
        // from `selected_sorted`, so the tags-at-HEAD no-op guard above is
        // intentionally bypassed for this mode.
        let dist = config.dist.clone();
        let run_opts = publish_only::RunOpts {
            dry_run: opts.dry_run,
        };
        // When --crate is given, prefer the matching per-crate dist
        // subdir (`dist/<crate>/`) when one exists so the publish reads
        // that crate's preserved manifests, its per-crate `Tag`, and its
        // workspace overlay — same shape the no-flag auto-iteration uses.
        // Fall back to the flat root when no subdir exists (single-crate
        // preserve laid down at the dist root).
        if !opts.crate_names.is_empty() {
            let with_subdir: Vec<String> = opts
                .crate_names
                .iter()
                .filter(|name| publish_only::crate_subdir_has_manifest(&dist, name, log))
                .cloned()
                .collect();
            if with_subdir.is_empty() {
                return publish_only::run(ctx, config, log, run_opts);
            }
            // Fail closed on a partial match. When SOME requested crates
            // have a per-crate subdir and some don't, silently publishing
            // only the matched subset before an irreversible publish would
            // be a quiet scope reduction — the operator asked for crates
            // that aren't represented in the preserved dist. Name the
            // missing ones so they can fix the request or re-run preserve.
            if with_subdir.len() != opts.crate_names.len() {
                let missing: Vec<&String> = opts
                    .crate_names
                    .iter()
                    .filter(|name| !with_subdir.iter().any(|s| s == *name))
                    .collect();
                anyhow::bail!(
                    "publish-only --crate: {} of {} requested crate(s) have no \
                     preserved per-crate dist at {} (missing: {}). The remaining \
                     crates ({}) do. Refusing to silently publish only the subset \
                     before an irreversible publish — re-run with only the crates \
                     that were preserved, or re-run the preserve step so every \
                     requested crate has a dist/<crate>/ subdir.",
                    missing.len(),
                    opts.crate_names.len(),
                    dist.display(),
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    with_subdir.join(", "),
                );
            }
            let all_known: Vec<CrateConfig> =
                config.crate_universe().into_iter().cloned().collect();
            let sorted = topo_sort_selected(&all_known, &with_subdir);
            let order = if sorted.is_empty() {
                with_subdir
            } else {
                sorted
            };
            return publish_only::run_per_crate(ctx, config, log, run_opts, dist, order);
        }
        // Detect layout and dispatch.
        match publish_only::detect_dist_layout(&dist, log)? {
            publish_only::DistLayout::Flat => {
                return publish_only::run(ctx, config, log, run_opts);
            }
            publish_only::DistLayout::PerCrate(subdirs) => {
                // Topo-sort discovered crate names so depends_on ordering
                // is respected. Fall back to alphabetical when none of the
                // discovered names match any configured crate — but only
                // when NO workspace overlay was applied: post-overlay the
                // universe is one workspace's crates, and a zero-match dist
                // means every discovered subdir is a SIBLING crate that
                // would publish under this workspace's env/signs/skip. Fail
                // closed instead, mirroring the --crate partial-match bail
                // above.
                let all_known: Vec<CrateConfig> =
                    config.crate_universe().into_iter().cloned().collect();
                let sorted = topo_sort_selected(&all_known, &subdirs);
                if sorted.is_empty()
                    && let Some(ref ws_name) = opts.workspace
                {
                    let ws_crates: Vec<&str> = all_known.iter().map(|c| c.name.as_str()).collect();
                    anyhow::bail!(
                        "publish-only --workspace {}: none of the preserved per-crate dist \
                         subdirs at {} ({}) belong to workspace '{}' (its crates: {}). \
                         Refusing to publish sibling crates under this workspace's \
                         env/signs/skip — check the workspace name, or point the run at \
                         the preserved dist produced for this workspace.",
                        ws_name,
                        dist.display(),
                        subdirs.join(", "),
                        ws_name,
                        if ws_crates.is_empty() {
                            "(none)".to_string()
                        } else {
                            ws_crates.join(", ")
                        },
                    );
                }
                let order = if sorted.is_empty() { subdirs } else { sorted };
                return publish_only::run_per_crate(ctx, config, log, run_opts, dist, order);
            }
            publish_only::DistLayout::Ambiguous { crate_subdirs } => {
                anyhow::bail!(
                    "publish-only: ambiguous dist layout at {} — found both a flat \
                     context.json at the root AND per-crate subdirectories ({}). \
                     Delete one or the other, or pass --crate <name> to select a \
                     specific crate.",
                    dist.display(),
                    crate_subdirs.join(", ")
                );
            }
        }
    }

    if opts.announce_only {
        return announce_only::run(ctx, config, log, opts.dry_run);
    }

    if opts.split {
        return split::run_split(ctx, config, log);
    }

    if opts.merge {
        return split::run_merge(ctx, config, log, opts.dry_run, None);
    }

    let p = pipeline::build_release_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        run_post_pipeline(ctx, config, opts.dry_run, log)?;
    }

    if result.is_ok() {
        gate_required_failures(ctx)?;
    }

    result
}
