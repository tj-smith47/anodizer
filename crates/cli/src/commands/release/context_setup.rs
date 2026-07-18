use super::*;

/// Decide whether the pre-flight publisher-state check should run.
///
/// Encodes the gating rules so they can be unit-tested without dragging
/// the entire pipeline up. The rules are:
///
/// - `--no-preflight` always wins → false.
/// - `--snapshot` / `--dry-run` / `--split` skip → no upstream side effects.
/// - `publish` in `skip` → caller opted out of one-way doors.
/// - otherwise → true. `--publish-only` runs it like a regular release: it
///   is the one mode that actually crosses the one-way doors, and the
///   probes (whoami / duplicate-version / moderation-queue / open-PR /
///   endpoint reachability) are read-only and cost seconds — the
///   config-derived env preflight has already validated the credentials
///   they use by the time this fires.
///
/// Note: this is the implicit-run decision. `--preflight` (the explicit
/// check-only mode) gates separately in the call site and always runs the
/// check independently of this predicate. `--announce-only` is handled by
/// an earlier short-circuit in `run_publisher_preflight` and so is not a
/// parameter here.
pub(crate) fn should_run_preflight_auto(
    no_preflight: bool,
    snapshot: bool,
    dry_run: bool,
    split: bool,
    publish_skipped: bool,
) -> bool {
    !no_preflight && !snapshot && !dry_run && !split && !publish_skipped
}

/// `--prepare`: runs local build/archive/sign/checksum/sbom stages but skips
/// everything that reaches upstream — the shared
/// [`anodizer_core::stages::UPSTREAM_STAGES`] classification (release,
/// docker build/push + signature push, blob, publish, snapcraft upload,
/// announce, post-publish verification), so the publish-nothing contract
/// cannot drift from the set the determinism harness also derives from.
/// Idempotent — won't duplicate stages already present in `skip`.
///
/// Composition with `--snapshot`: well-defined — `--prepare --snapshot` emits
/// snapshot-prefixed artifacts (`Version`/`Tag` derived from
/// `<version>-SNAPSHOT-<shortcommit>`, no tag required) without publishing.
/// Useful for generating pre-release archives in PR CI without needing a real
/// tag or release. `--prepare` without `--snapshot` requires a real tag.
pub(crate) fn apply_prepare_mode_to_skip(skip: &mut Vec<String>) {
    for &stage in anodizer_core::stages::UPSTREAM_STAGES {
        if !skip.iter().any(|s| s == stage) {
            skip.push(stage.to_string());
        }
    }
}

/// Installs the pre-submitter verify-release gate onto `ctx.verify_gate`.
/// Extracted to a named seam (rather than an inline closure at the call
/// site) so wiring — not just [`anodizer_stage_verify_release::run_asset_gate`]'s
/// own behavior, which is unit-tested directly in that crate — has its own
/// falsifiable test: deleting the call to this function, or swapping it for
/// a decoy, must fail a test rather than silently pass the whole tree.
pub(crate) fn install_verify_gate(ctx: &mut Context) {
    ctx.verify_gate = Some(std::sync::Arc::new(|ctx: &mut Context| {
        anodizer_stage_verify_release::run_asset_gate(ctx)
    }));
}

/// `--strict` and `--allow-nondeterministic` are mutually exclusive: strict
/// mode forbids the determinism stage from suppressing findings, the
/// allowlist's whole purpose is to suppress one. clap can't express this
/// directly (--strict lives on the top-level Cli struct and the allowlist on
/// the Release variant), so the check runs here.
pub(crate) fn validate_strict_vs_allowlist(opts: &ReleaseOpts) -> Result<()> {
    if opts.strict && !opts.allow_nondeterministic.is_empty() {
        anyhow::bail!(
            "--strict and --allow-nondeterministic are mutually exclusive (drop --strict if a runtime exemption is required)"
        );
    }
    Ok(())
}

/// Apply the workspace overlay (explicit `--workspace`, or inferred from the
/// `--crate` selection when it resolves into a single workspace). Returns the
/// list of workspace-level skip stages to merge later. Delegates to the
/// shared [`helpers::apply_workspace_scope`] so `release`, `build`, and every
/// other crate-selecting command scope and validate identically.
pub(crate) fn apply_workspace_overlay_for_opts(
    config: &mut Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<Vec<String>> {
    helpers::apply_workspace_scope(config, opts.workspace.as_deref(), &opts.crate_names, log)
}

/// Apply CLI overrides that mutate `config.release` (draft / header / footer
/// and their `_tmpl` variants). `*_tmpl` flags override their plain
/// counterparts; the template stage renders the content later.
pub(crate) fn apply_release_meta_overrides(config: &mut Config, opts: &ReleaseOpts) -> Result<()> {
    if opts.draft {
        let release = config.release.get_or_insert_with(Default::default);
        release.draft = Some(true);
    }
    if let Some(ref header_path) = opts.release_header {
        let header_content = std::fs::read_to_string(header_path).with_context(|| {
            format!(
                "failed to read release header file: {}",
                header_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.header = Some(anodizer_core::config::ContentSource::Inline(header_content));
    }
    if let Some(ref header_tmpl_path) = opts.release_header_tmpl {
        let raw = std::fs::read_to_string(header_tmpl_path).with_context(|| {
            format!(
                "failed to read release header template file: {}",
                header_tmpl_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.header = Some(anodizer_core::config::ContentSource::Inline(raw));
    }
    if let Some(ref footer_path) = opts.release_footer {
        let footer_content = std::fs::read_to_string(footer_path).with_context(|| {
            format!(
                "failed to read release footer file: {}",
                footer_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(anodizer_core::config::ContentSource::Inline(footer_content));
    }
    if let Some(ref footer_tmpl_path) = opts.release_footer_tmpl {
        let raw = std::fs::read_to_string(footer_tmpl_path).with_context(|| {
            format!(
                "failed to read release footer template file: {}",
                footer_tmpl_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(anodizer_core::config::ContentSource::Inline(raw));
    }
    Ok(())
}

/// Enforce the dist directory state: `--clean` removes it (logs in dry-run);
/// otherwise a populated dist is a hard error.
/// `--merge` / `--publish-only` / `--rollback-only` skip the non-empty check
/// because each of those modes requires preserved dist content;
/// `--preflight-secrets` skips it because the secrets gate is a
/// zero-mutation check that never reads or writes dist.
pub(crate) fn enforce_dist_state(
    config: &Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<()> {
    if opts.clean && !opts.dry_run {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    } else if opts.clean && opts.dry_run {
        log.status("(dry-run) would clean dist directory");
    }

    if !opts.clean
        && !opts.merge
        && !opts.publish_only
        && !opts.rollback_only
        && !opts.announce_only
        && !opts.preflight_secrets
    {
        let dist = &config.dist;
        if dist.exists()
            && let Ok(mut entries) = dist.read_dir()
            && entries.next().is_some()
        {
            return Err(anodizer_core::error_class::deterministic_msg(format!(
                "dist directory '{}' is not empty; use --clean to remove it first",
                dist.display()
            )));
        }
    }
    Ok(())
}
/// Read the `--release-notes-tmpl` file (when set) so its content can be
/// rendered post-`populate_*_vars`. `--release-notes-tmpl` overrides
/// `--release-notes`.
pub(crate) fn read_release_notes_template(opts: &ReleaseOpts) -> Result<Option<(PathBuf, String)>> {
    if let Some(ref tmpl_path) = opts.release_notes_tmpl {
        let content = std::fs::read_to_string(tmpl_path).with_context(|| {
            format!(
                "failed to read release notes template: {}",
                tmpl_path.display()
            )
        })?;
        Ok(Some((tmpl_path.clone(), content)))
    } else {
        Ok(None)
    }
}

/// Translate `--rollback=<v>` into the enum; reject invalid values up front
/// so the dispatch site can rely on a clean value.
pub(crate) fn parse_rollback_mode(rollback: Option<&str>) -> Result<Option<RollbackMode>> {
    match rollback {
        Some("none") => Ok(Some(RollbackMode::None)),
        Some("best-effort") => Ok(Some(RollbackMode::BestEffort)),
        Some(other) => anyhow::bail!(
            "invalid --rollback value: {} (expected: none, best-effort)",
            other
        ),
        None => Ok(None),
    }
}

/// Resolve the `--simulate-failure` list. The flag is test-only and gated by
/// `ANODIZE_TEST_HARNESS=1`; production releases that accidentally set the
/// flag get a hard error rather than silent pass-through so the surface
/// cannot be weaponized.
pub(crate) fn resolve_simulate_failure(simulate: &mut Vec<String>) -> Result<Vec<String>> {
    if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") {
        Ok(std::mem::take(simulate))
    } else if !simulate.is_empty() {
        anyhow::bail!(
            "--simulate-failure requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)"
        );
    } else {
        Ok(Vec::new())
    }
}

/// Translate `--allow-nondeterministic name=reason` (repeatable) into
/// `(name, reason)` tuples. Empty reasons are rejected so the run summary
/// always carries a human-readable justification.
pub(crate) fn parse_allow_nondeterministic(entries: &[String]) -> Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|s| {
            let (name, reason) = s.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("--allow-nondeterministic must be NAME=REASON, got: {}", s)
            })?;
            if reason.trim().is_empty() {
                anyhow::bail!("--allow-nondeterministic reason cannot be empty for: {}", s);
            }
            Ok::<_, anyhow::Error>((name.to_string(), reason.to_string()))
        })
        .collect()
}

/// Resolve `project_root` for [`ContextOptions::project_root`].
///
/// Precedence:
///   1. The parent directory of the resolved config file (authoritative
///      — the operator may have invoked anodizer from a subdirectory
///      with `--config=../anodize.yaml`).
///   2. Process CWD (`current_dir`), as a fallback when the config path
///      lacks a parent component (e.g. a bare filename in `/`).
///
/// Both branches canonicalize when possible so downstream consumers that
/// join repo-relative paths (snapcraft icons, extra-file globs, ...) hit
/// the real tree even when called from a symlinked checkout.
///
/// When the CWD fallback fires (bare-filename `--config=anodize.yaml`)
/// and `log` is `Some`, a warn surfaces because the resulting CWD
/// anchor is almost certainly NOT what the operator meant when they
/// passed a bare filename: repo-relative file lookups (snapcraft icon
/// resolution, extra-file globs, etc.) will all hit the process CWD
/// rather than the repo root. We warn rather than bail because
/// legitimate workflows do invoke anodizer with CWD == project root and
/// a bare filename; the warn lets a misconfiguration become visible
/// without breaking the working case.
pub(crate) fn resolve_project_root(
    config_path: &std::path::Path,
    log: Option<&StageLogger>,
) -> Option<PathBuf> {
    let from_parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf);
    let candidate = match from_parent {
        Some(p) => p,
        None => {
            let cwd = std::env::current_dir().ok()?;
            if let Some(log) = log {
                log.warn(&format!(
                    "project_root falling back to CWD `{}` because --config=`{}` is a bare filename",
                    cwd.display(),
                    config_path.display()
                ));
                log.warn(
                    "repo-relative file lookups (snapcraft icons, extra-file globs, ...) \
                     will resolve against the process CWD — pass --config with a parent \
                     directory (e.g. `--config=./anodize.yaml`) if this is incorrect",
                );
            }
            cwd
        }
    };
    Some(std::fs::canonicalize(&candidate).unwrap_or(candidate))
}

/// Resolve `--host-targets` into `opts.targets` against the detected host
/// triple. Thin wrapper over [`apply_host_targets_filter`] that supplies the
/// real host via `rustc -vV`; the filter itself is host-injectable so its
/// per-config-mode behaviour can be unit-tested deterministically.
pub(crate) fn resolve_host_targets(
    opts: &mut ReleaseOpts,
    config: &Config,
    selected_crates: &[String],
    log: &StageLogger,
) -> Result<()> {
    let host = anodizer_core::partial::resolve_host_target()
        .context("--host-targets: failed to detect the host target triple")?;
    apply_host_targets_filter(opts, config, selected_crates, &host, log)
}

/// Partition the configured target union for `selected_crates` into the
/// host-buildable subset and write it to `opts.targets`.
///
/// Collects the union (honoring per-build `targets`, `defaults.targets`, and
/// `builds.ignore` exactly as the build stage does — so every config mode,
/// single-crate / workspace-lockstep / workspace-per-crate, resolves the same
/// list the builds will), partitions it via
/// [`anodizer_core::partial::host_buildable_targets`] against `host`, logs the
/// skipped set once, and feeds the kept set through the existing
/// `PartialTarget::Targets` intersection filter.
///
/// Hard-errors when the host can build NONE of the configured targets
/// (e.g. an apple-darwin-only config on a Linux host): proceeding would
/// emit an empty snapshot that breaks the downstream archive / checksum
/// stages, so the operator is told which native host each skipped group
/// requires (a macOS host for apple targets, a Windows host for
/// windows-msvc) rather than a hardcoded single remedy.
pub(crate) fn apply_host_targets_filter(
    opts: &mut ReleaseOpts,
    config: &Config,
    selected_crates: &[String],
    host: &str,
    log: &StageLogger,
) -> Result<()> {
    let configured = helpers::collect_build_targets(config, selected_crates);

    // A config with no build targets at all has nothing to filter; leave
    // `opts.targets` untouched so downstream stages handle the no-build case
    // (e.g. lib-only crates) exactly as they would without --host-targets.
    if configured.is_empty() {
        return Ok(());
    }

    let (kept, skipped) = anodizer_core::partial::host_buildable_targets(host, &configured);

    if let Some(msg) = anodizer_core::partial::host_targets_skip_message(host, &skipped) {
        log.warn(&msg);
    }

    if kept.is_empty() {
        // Every configured target was skipped — name the native host each
        // group needs (reusing the grouped skip clauses) rather than a
        // hardcoded macOS remedy, which would mislead a windows-msvc-only
        // config skipped purely for lack of a Windows host.
        let reasons = anodizer_core::partial::host_targets_skip_reasons(host, &skipped);
        anyhow::bail!(
            "--host-targets: none of the {} configured target(s) can be built on this host \
             ({}); all require a different native host: {}. Adjust build.targets, or run on \
             a host that satisfies the constraint above.",
            configured.len(),
            host,
            reasons,
        );
    }

    opts.targets = Some(kept);
    Ok(())
}

/// Assemble the [`ContextOptions`] from parsed flags + derived state.
/// `resume_release` auto-enables under `--publish-only` so the publish
/// pipeline's `ReleaseStage` and `github-release` publisher target the same
/// tag without tripping the leftover-asset bail.
///
/// `project_root` resolves from the parent directory of the resolved
/// config file when available, falling back to the process CWD. The
/// resolved config path is authoritative because the operator may have
/// invoked anodizer from a subdirectory with `--config=../anodize.yaml`;
/// CWD alone would point repo-relative consumers at the wrong tree.
/// Stage modules that need to read repo-relative files (snapcraft
/// icons, extra-file globs, the cargo publisher's `target/`
/// resolution, ...) consume this via `ctx.options.project_root`.
pub(crate) fn build_context_options(
    opts: &ReleaseOpts,
    skip_stages: Vec<String>,
    selected_sorted: Vec<String>,
    rollback_mode: Option<RollbackMode>,
    simulate_failure_publishers: Vec<String>,
    runtime_nondeterministic_allowlist: Vec<(String, String)>,
    project_root: Option<PathBuf>,
) -> ContextOptions {
    ContextOptions {
        snapshot: opts.snapshot,
        nightly: opts.nightly,
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages,
        selected_crates: selected_sorted,
        token: opts.token.clone(),
        parallelism: opts.parallelism,
        single_target: opts.single_target.clone(),
        release_notes_path: opts.release_notes.clone(),
        fail_fast: opts.fail_fast,
        partial_target: opts
            .targets
            .clone()
            .map(anodizer_core::partial::PartialTarget::Targets),
        merge: opts.merge,
        publish_only: opts.publish_only,
        preflight_secrets: opts.preflight_secrets,
        project_root,
        strict: opts.strict,
        strict_preflight: opts.strict_preflight,
        resume_release: opts.resume_release || opts.publish_only,
        replace_existing_artifacts: opts.replace_existing,
        skip_post_publish_poll: opts.no_post_publish_poll,
        gate_submitter: if opts.no_gate_submitter {
            Some(false)
        } else {
            None
        },
        rollback_mode,
        simulate_failure_publishers,
        rollback_only: opts.rollback_only,
        allow_rerun: opts.allow_rerun,
        show_skipped: opts.show_skipped,
        from_run: opts.from_run.clone(),
        runtime_nondeterministic_allowlist,
        summary_json_path: opts.summary_json.clone(),
        allow_ai_failure: opts.allow_ai_failure,
        // The full release pipeline has no `--from`; changelog range starts
        // are auto-discovered per crate. Only `anodizer changelog --from`
        // sets this.
        changelog_from: None,
        // The full release pipeline never spans full history; each crate's
        // notes bound at its previous tag. Only `anodizer changelog ..` opts in.
        changelog_full_history: false,
        // The full release pipeline bounds each crate's notes at its previous
        // tag, walking to HEAD; only the standalone `changelog <from>..<to>`
        // command pins an explicit upper bound.
        changelog_to: None,
        // The release pipeline is NOT a local preview: its tag-at-HEAD,
        // dirty-tree, snapshot-gate, and github-native guards must all stay
        // intact. Only the standalone `changelog --format release-notes`
        // command sets this true.
        changelog_preview: false,
        notify: false,
        allow_snapshot_publish: opts.allow_snapshot_publish,
        publisher_allowlist: opts.publishers.clone(),
    }
}
