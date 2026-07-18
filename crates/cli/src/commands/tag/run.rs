use super::*;

pub fn run(mut opts: TagOpts) -> Result<()> {
    // Discover the workspace root once, config-derived, so `tag` resolves the
    // same root whether invoked from the repo root or a subdirectory (matching
    // `bump` and `changelog`); every workspace-load / git-working-dir site below
    // threads this one value instead of re-reading the cwd.
    let workspace_root_path =
        crate::commands::helpers::discover_workspace_root(resolve_config_path(&opts).as_deref())?;
    // Load the full config + Cargo workspace once so all downstream helpers
    // share the same parse (eliminates the previous triple workspace-file
    // read on lockstep repos). Resolved at the discovered workspace root so a
    // subdirectory invocation still finds the repo-root `.anodizer.yaml` and its
    // `version_files` enrollment, not just whatever sits in the cwd.
    let loaded_config: anodizer_core::config::Config = load_config_at(&opts, &workspace_root_path)?;
    let loaded_workspace: Option<WorkspaceInfo> = load_workspace(&workspace_root_path)?;

    // `--crate` naming the shared-root aggregate itself selects the same unit
    // a bare `tag` releases (the aggregate's name never appears in the crate
    // universe), so the flag is dropped and the run proceeds exactly as an
    // unfiltered tag. Rejecting it would break scripted `tag --crate $PROJECT`
    // invocations on lockstep repos, where that spelling has always meant
    // repo-level tagging.
    if let Some(ref name) = opts.crate_name
        && shared_root_aggregate_name(
            &workspace_root_path,
            &loaded_config,
            loaded_workspace.as_ref(),
        ) == Some(name.as_str())
    {
        opts.crate_name = None;
    }

    let tag_config = loaded_config.tag.clone().unwrap_or_default();
    let git_config: Option<anodizer_core::config::GitConfig> = loaded_config.git.clone();

    // tag_pre_hooks / tag_post_hooks apply to EVERY tagging shape; extracted
    // before shape dispatch so the per-crate engine receives them too.
    let pre_hooks = tag_config.tag_pre_hooks.clone().unwrap_or_default();
    let post_hooks = tag_config.tag_post_hooks.clone().unwrap_or_default();

    // Refresh CHANGELOG.md into the version-bump commit (riding the same
    // `git add` as the Cargo.toml / version_files edits) when `changelog:` is
    // configured and not skipped — `tag` is what release CI runs, so without
    // this the changelogs rot between releases even though `bump` refreshes them.
    let changelog_enabled = resolve_changelog_enabled(Some(&loaded_config), opts.changelog);

    // Reject an incoherent flat-aggregate config (members sharing one tag prefix
    // but disagreeing on `[package].version`) before any work, identically to
    // `changelog` and `bump`.
    guard_flat_aggregate_coherence(
        Some(&loaded_config),
        loaded_workspace.as_ref(),
        &workspace_root_path,
    )?;

    let mut cfg = ResolvedConfig::from_tag_config(&tag_config, &opts);

    // Validate + normalize the explicit `--version` override once, up front, so
    // an ill-formed value fails before any git/manifest work. The bare
    // `MAJOR.MINOR.PATCH[-pre][+build]` form is retained (the configured tag
    // prefix is re-applied at tag-creation time); accepting both `1.2.3` and
    // `v1.2.3` is exactly `parse_semver`'s contract.
    let version_override: Option<String> = match opts.version_override.as_deref() {
        Some(raw) => {
            let sv = git::parse_semver(raw).map_err(|_| {
                anyhow::anyhow!("--version {:?} is not a valid semver version", raw)
            })?;
            Some(sv.version_string())
        }
        None => None,
    };

    // Push controls shared by every tagging path. `remote` defaults to origin;
    // `effective_push` per-path resolution is computed at each call site so the
    // per-crate path can carry its own (true) default.
    let remote = opts.push_remote.as_deref().unwrap_or("origin").to_string();
    let config_push = tag_config.push;
    // Signed-tag selection is workspace-global: resolved once here and threaded
    // to every tag-creation call site (single/lockstep closure, github-api
    // fallback, and the per-crate engine) so no dispatch shape can diverge.
    let effective_sign = resolve_effective_sign(&opts, tag_config.sign);

    // When --crate is given, look up the crate in config and derive the tag
    // prefix from its tag_template.  Also capture the crate path to
    // scope change detection to only that directory.
    //
    // An unknown name is a hard error, never a fall-through: the repo-level
    // path below tags with the default `v` prefix and NO path scoping, so a
    // typo'd `--crate` would cut (and possibly push) a wrong repo-wide tag.
    // A KNOWN crate whose template has no extractable prefix is a distinct
    // condition and stays valid — it tags under the canonical `<name>-v`
    // fallback family (`git::per_crate_tag_prefix`), same as the per-crate
    // engine and the changelog's crate selection.
    let mut crate_path: Option<String> = None;
    let mut version_sync_enabled = false;
    let mut crate_version_files: Vec<String> = Vec::new();
    if let Some(ref crate_name) = opts.crate_name {
        crate::commands::helpers::validate_selection_against_universe(
            &loaded_config,
            std::slice::from_ref(crate_name),
            None,
        )?;
        let info = load_crate_tag_info(&loaded_config, crate_name).ok_or_else(|| {
            anyhow::anyhow!(
                "--crate '{}': crate resolved but its tag info could not be loaded",
                crate_name
            )
        })?;
        cfg.tag_prefix = info.tag_prefix;
        crate_path = Some(info.path);
        version_sync_enabled = info.version_sync;
        crate_version_files = info.version_files;
    }

    // Repo shape drives every multi-crate decision below — the
    // custom-tag/--version validation errors, the `release_branches`
    // guard's output dialect, and the per-crate dispatch itself. Resolve
    // it ONCE, and fold it into the ONE dispatch decision
    // (`dispatch_groups`), so those surfaces can never disagree.
    let repo_shape = detect_repo_shape(
        &workspace_root_path,
        Some(&loaded_config),
        loaded_workspace.as_ref(),
    );

    // custom_tag is incompatible with per-crate mode: the whole point of a
    // custom tag is to override version computation for one unit. In per-crate
    // mode there is no single unit — use --crate to target a specific crate.
    if let Some(ref ct) = cfg.custom_tag
        && opts.crate_name.is_none()
        && matches!(repo_shape, RepoShape::PerCrate(_))
    {
        anyhow::bail!(
            "--custom-tag {:?} is incompatible with per-crate workspace mode; \
             pass --crate <name> to override a single crate's tag",
            ct
        );
    }

    // `--version` pins ONE version. In per-crate / flat-aggregate dispatch there
    // is no single versioned unit — applying one version across independently
    // versioned crates would corrupt their cadences — so reject it unless
    // `--crate <name>` narrows to a single crate (which routes through the
    // single-crate derivation path below where the override is honored).
    if let Some(ref v) = version_override
        && opts.crate_name.is_none()
        && matches!(
            repo_shape,
            RepoShape::PerCrate(_) | RepoShape::FlatAggregate(_)
        )
    {
        anyhow::bail!(
            "--version {:?} is incompatible with per-crate workspace mode; \
             pass --crate <name> to pin a single crate's version",
            v
        );
    }

    // Per-crate / hybrid-workspace dispatch decision: when no --crate is given
    // and the repository has per-crate versions (anodizer workspaces: or
    // multiple crates: entries without a lockstep [workspace.package].version),
    // delegate to the multi-crate handler which runs change detection, bumps
    // all selected crates in one commit, creates per-crate tags, and pushes
    // atomically.
    //
    // A `FlatAggregate` (shared-prefix flat `crates:` list, no
    // `[workspace.package].version`) has its versions in N per-crate
    // `[package].version` manifests, so it is bumped by the per-crate engine
    // applied as ONE group: that creates the single shared tag (its built-in
    // dedup) and the one collapsed root changelog section. A custom tag names
    // ONE explicit tag for the shared unit; it is honored by the lockstep
    // custom-tag fall-through below, not the per-crate engine (which ignores
    // `custom_tag`), so a `FlatAggregate` WITH a custom tag stays out of the
    // group dispatch.
    let mut is_flat_aggregate = false;
    let dispatch_groups: Option<Vec<Vec<CrateConfig>>> = if opts.crate_name.is_none() {
        match repo_shape {
            RepoShape::PerCrate(groups) => Some(groups),
            RepoShape::FlatAggregate(crates) if cfg.custom_tag.is_none() => {
                is_flat_aggregate = true;
                Some(vec![crates])
            }
            // Genuine Cargo-workspace lockstep, single-crate repos, and a
            // `FlatAggregate` carrying a custom tag keep the existing
            // fall-through paths below.
            RepoShape::Single | RepoShape::Lockstep | RepoShape::FlatAggregate(_) => None,
        }
    } else {
        None
    };

    // Merge verbose from config (config verbose=true enables verbose unless
    // the CLI says quiet); built once here so the guard and both dispatch
    // paths share one logger.
    let config_verbose = tag_config.verbose.unwrap_or(false);
    let effective_verbose = opts.verbose || (config_verbose && !opts.quiet);
    let log = StageLogger::new(
        "tag",
        Verbosity::from_flags(opts.quiet, effective_verbose, opts.debug),
    );

    // Previous-tag resolution must reflect the REMOTE's tag reality, not this
    // clone's: a tag deleted on the remote for a re-cut can survive locally
    // (in this or another clone) and would otherwise silently mint the NEXT
    // version instead of re-minting the SAME one. One ls-remote call per
    // invocation; every previous-tag lookup below shares the result. A network
    // failure falls back to local tags with a warning rather than blocking
    // offline tagging.
    let remote_tag_names: Option<std::collections::HashSet<String>> =
        if git::has_remote_in(&workspace_root_path, &remote) {
            match git::list_remote_tag_names_in(&workspace_root_path, &remote) {
                Ok(names) => Some(names.into_iter().collect()),
                Err(e) => {
                    log.warn(&format!(
                        "could not list tags on remote '{remote}' ({e}); previous-tag \
                         resolution is falling back to LOCAL tags — a local tag that was \
                         deleted on the remote may mint the wrong next version"
                    ));
                    None
                }
            }
        } else {
            None
        };

    // `release_branches` guard, BEFORE shape dispatch so it protects every
    // tagging shape uniformly: a feature-branch run that opts into `--push`
    // could otherwise cut and push live tags off a non-release branch.
    // Bypass semantics match the single/lockstep path:
    // an explicit `--version` is an authoritative "tag exactly this" request,
    // and a custom tag has always been created before the branch check ran.
    // The output dialect follows the SAME dispatch decision the real dispatch
    // below consumes.
    if version_override.is_none() && cfg.custom_tag.is_none() && !cfg.release_branches.is_empty() {
        let current_branch = git::get_current_branch()?;
        if !branch_matches(&current_branch, &cfg.release_branches) {
            if dispatch_groups.is_some() {
                // Per-crate consumers parse the `anodizer-output` lines; empty
                // payloads are the established "nothing tagged" shape.
                log.status(&format!(
                    "branch '{}' is not a release branch; skipping per-crate tagging",
                    current_branch
                ));
                println!("anodizer-output crates=[]");
                println!("anodizer-output versions={{}}");
                return Ok(());
            }
            // Non-release branch: produce a hash-postfixed version, don't tag.
            let short_commit = git::get_short_commit()?;
            let prev_tag = find_previous_tag(&cfg, git_config.as_ref(), remote_tag_names.as_ref())?;
            let base_version = match &prev_tag {
                Some(tag) => {
                    let sv = git::parse_semver_tag(tag)?;
                    format!("{}.{}.{}", sv.major, sv.minor, sv.patch)
                }
                None => cfg.initial_version.clone(),
            };
            let hash_tag = format!("{}{}-{}", cfg.tag_prefix, base_version, short_commit);
            log.verbose(&format!(
                "branch '{}' is not a release branch, producing hash-postfixed version: {}",
                current_branch, hash_tag
            ));
            println!("new_tag={}", hash_tag);
            println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
            println!("part=none");
            return Ok(());
        }
    }

    // Submitter moderation-queue advisories are verbose-only; emit them once
    // off the single load (hidden at the default log level).
    crate::pipeline::emit_config_advisories(&loaded_config, &log);

    if let Some(groups) = dispatch_groups {
        // Faithful GitHub-API tagging is incompatible with the per-crate
        // engine's atomic branch+multi-tag push (per-tag API calls cannot
        // be atomic with the bump commit); silently falling back to local
        // tagging would ignore explicit config, so fail loudly instead.
        if cfg.git_api_tagging {
            anyhow::bail!(
                "git_api_tagging: true is not supported with per-crate tagging \
                 (per-crate tags are pushed atomically with the bump commit); \
                 remove git_api_tagging or use --crate <name> to tag one crate"
            );
        }
        log.status(&format!(
            "running auto-tag (per-crate){}",
            if opts.dry_run { " (dry-run)" } else { "" }
        ));
        return run_per_crate_tag(
            PerCrateDispatch {
                groups,
                is_flat_aggregate,
                workspace_root: workspace_root_path.clone(),
            },
            &opts,
            &cfg,
            git_config.as_ref(),
            Some(&loaded_config),
            PushControls {
                remote: &remote,
                config_push,
                sign: effective_sign,
                changelog_enabled,
                pre_hooks: &pre_hooks,
                post_hooks: &post_hooks,
                remote_tags: remote_tag_names.as_ref(),
            },
            &log,
        );
    }

    // Workspace-mode: with no --crate, treat a Cargo workspace whose members
    // inherit [workspace.package].version as a single versioned unit. The
    // tag-derived version gets applied to root Cargo.toml + every member
    // manifest + workspace.dependencies pins before the tag is created, so
    // the tagged commit has Cargo.toml at the version the tag advertises.
    let workspace_info: Option<&WorkspaceInfo> = if opts.crate_name.is_none() {
        loaded_workspace
            .as_ref()
            .filter(|ws| ws.workspace_package_version.is_some())
    } else {
        None
    };

    log.status(&format!(
        "running auto-tag{}",
        if opts.dry_run { " (dry-run)" } else { "" }
    ));

    // Helper closure to create a tag via the appropriate method, with
    // tag_pre_hooks / tag_post_hooks wrapping. Hooks receive template vars
    // `{{ .Tag }}`, `{{ .PrefixedTag }}`, `{{ .Version }}`, `{{ .PreviousTag }}`
    // and process env `ANODIZER_CURRENT_TAG` / `ANODIZER_PREVIOUS_TAG`.
    let strict = opts.strict;
    let tag_prefix_for_hooks = cfg.tag_prefix.clone();

    // A bare run is fully LOCAL (tag + bump commit both stay in the clone) in
    // every dispatch shape. `--push`, `tag.push=true`, or `--push-dry-run`
    // (preview) opt into pushing the bump commit (the branch HEAD) atomically
    // with the tag. A tag pushed without its bump commit (an orphan tag) is
    // only producible through the explicit `--push-tags-only` opt-in, never as
    // a silent default.
    //
    // `--push-dry-run` previews the push commands `--push` would run: treat it
    // as push-mode-on, but every `git push` is replaced by a "(dry-run) would
    // push …" log line.
    // `--push-tags-only` overrides `tag.push = true` config (an explicit CLI
    // choice beats persisted config, matching --no-push) and combines with
    // `--push-dry-run` as a preview of the tags-only push.
    let push_mode =
        !opts.push_tags_only && (resolve_effective_push(&opts, config_push) || opts.push_dry_run);

    // A signed tag and API tagging on a pushed tag are mutually exclusive: the
    // API path mints the tag object on the remote, where the user's local
    // GPG/SSH signing key cannot reach it, so honoring `--sign` there would
    // silently ship an UNSIGNED tag. Since signing is opt-in, an explicit
    // signature request must hard-error rather than downgrade.
    if effective_sign && cfg.git_api_tagging && push_mode {
        anyhow::bail!(
            "signed tags (--sign / tag.sign) are incompatible with git_api_tagging \
             on a pushed tag: the GitHub API creates the tag object on the remote and \
             cannot apply your local GPG/SSH signature. Remove git_api_tagging to \
             create a signed tag locally, or drop --sign / tag.sign."
        );
    }

    let push_preview = opts.push_dry_run;
    let push_branch = if push_mode {
        Some(git::get_current_branch()?)
    } else {
        None
    };

    // The git working dir is the discovered workspace root — bind once and
    // reuse across every tag so git ops run from the repo root even when the
    // command was invoked from a subdirectory.
    let cwd = workspace_root_path.clone();

    let create_tag = |tag: &str, message: &str, dry_run: bool, prev: Option<&str>| -> Result<()> {
        let version = tag
            .strip_prefix(tag_prefix_for_hooks.as_str())
            .unwrap_or(tag);
        let tv = tag_hook_context(tag, version, prev);

        if !pre_hooks.is_empty() {
            run_hooks(
                &pre_hooks,
                "tag-pre",
                HookRunContext::new(dry_run, &log, Some(&tv)),
            )?;
        }

        // Whether the actual push step runs in dry-run/preview mode (creates
        // the tag locally but only prints the push commands).
        let push_dry = dry_run || push_preview;

        // The API tags a commit by SHA on the remote, so it can only run once
        // the bump commit is actually pushed — i.e. under a full branch+tag
        // push. Bare and --push-tags-only invocations fall through to the git
        // paths below: bare stays fully local (pushes nothing), and
        // --push-tags-only pushes the tag object via git (the API cannot
        // reference a commit the remote doesn't yet have).
        if cfg.git_api_tagging && push_mode {
            log.verbose("using GitHub API for tagging (git_api_tagging=true)");
            // Push the branch first so the bump commit lands on the remote,
            // THEN create the tag via the API (which references the
            // now-pushed HEAD commit).
            git::push_branch_and_tags_atomic_in(
                &cwd,
                &git::AtomicPushSpec {
                    remote: &remote,
                    branch: push_branch.as_deref(),
                    tags: &[],
                    dry_run: push_dry,
                    strict,
                },
                &log,
            )?;
            // Resolve the repo identity once (config override -> origin
            // remote) and hand it to the API tagger so it agrees with the
            // rest of the pipeline instead of re-parsing the remote itself.
            let release_github = loaded_config
                .release
                .as_ref()
                .and_then(|r| r.github.as_ref());
            let slug = git::resolve_github_slug_in(
                release_github.map(|g| g.owner.as_str()),
                release_github.map(|g| g.name.as_str()),
                &cwd,
            )?;
            git::create_tag_via_github_api_in(
                &cwd,
                std::path::Path::new("gh"),
                &slug,
                tag,
                message,
                // The API creates the tag ref on the REMOTE, so it is a push
                // operation: honour push-preview (`--push-dry-run`) as well as
                // `--dry-run`. Passing the closure's `dry_run` here would make
                // the real `gh api` call during a preview and orphan the tag on
                // a commit no pushed branch contains.
                push_dry,
                effective_sign,
                &log,
                strict,
            )?;
        } else if push_mode {
            // Create the tag locally, then push branch + tag atomically so
            // neither an orphan tag NOR an orphan bump commit is possible.
            git::create_tag_local_only(&cwd, tag, message, dry_run, effective_sign, &log)?;
            git::push_branch_and_tags_atomic_in(
                &cwd,
                &git::AtomicPushSpec {
                    remote: &remote,
                    branch: push_branch.as_deref(),
                    tags: std::slice::from_ref(&tag.to_string()),
                    dry_run: push_dry,
                    strict,
                },
                &log,
            )?;
        } else if opts.push_tags_only {
            // Deferred-branch pattern: the tag goes up now (triggering
            // tag-driven CI), the branch is advanced onto the bump commit by
            // the caller after publish succeeds.
            if cfg.git_api_tagging {
                log.verbose(
                    "git_api_tagging is set but --push-tags-only pushes the tag before its \
                     commit is on the remote; using git tag push (the API cannot reference \
                     an unpushed commit)",
                );
            }
            git::create_tag_local_only(&cwd, tag, message, dry_run, effective_sign, &log)?;
            git::push_branch_and_tags_atomic_in(
                &cwd,
                &git::AtomicPushSpec {
                    remote: &remote,
                    branch: None,
                    tags: std::slice::from_ref(&tag.to_string()),
                    dry_run: push_dry,
                    strict,
                },
                &log,
            )?;
        } else {
            // No push selected: everything stays local. Pushing the tag here
            // without the branch would orphan the bump commit on the remote.
            git::create_tag_local_only(&cwd, tag, message, dry_run, effective_sign, &log)?;
        }

        if !post_hooks.is_empty() {
            run_hooks(
                &post_hooks,
                "tag-post",
                HookRunContext::new(dry_run, &log, Some(&tv)),
            )?;
        }
        Ok(())
    };

    // If custom_tag is set, use it directly
    if let Some(ref custom) = cfg.custom_tag {
        let new_tag = if custom.starts_with(&cfg.tag_prefix) {
            custom.clone()
        } else {
            format!("{}{}", cfg.tag_prefix, custom)
        };
        log.verbose(&format!("using custom tag {}", new_tag));
        let prev_for_custom =
            find_previous_tag(&cfg, git_config.as_ref(), remote_tag_names.as_ref())
                .ok()
                .flatten();
        create_tag(
            &new_tag,
            &format!("Release {}", new_tag),
            opts.dry_run,
            prev_for_custom.as_deref(),
        )?;
        println!("new_tag={}", new_tag);
        println!("old_tag=");
        println!("part=custom");
        return Ok(());
    }

    // The `release_branches` guard already ran before shape dispatch (hoisted
    // so every tagging shape shares it); from here the branch is a release
    // branch or the guard was bypassed by --version / custom_tag.

    // Find previous tag
    let prev_tag = find_previous_tag(&cfg, git_config.as_ref(), remote_tag_names.as_ref())?;

    log.verbose(&format!(
        "previous tag = {}",
        prev_tag.as_deref().unwrap_or("(none)")
    ));

    // Check for changes since last tag.  When a crate path is known, scope
    // to that directory so unrelated commits don't trigger a spurious bump.
    if let Some(ref tag) = prev_tag {
        let has_changes = if let Some(ref path) = crate_path {
            git::has_changes_since_in(&workspace_root_path, tag, path)?
        } else {
            git::has_commits_since_tag(tag)?
        };
        if !has_changes {
            // An explicit `--version` is an authoritative release request, so it
            // forces past the "no changes since last tag" skip the same way
            // `force_without_changes` does (release-recovery re-tags often carry
            // no new commits).
            let force = version_override.is_some()
                || if cfg.prerelease {
                    cfg.force_without_changes_pre
                } else {
                    cfg.force_without_changes
                };
            if !force {
                log.verbose(&format!("skipped tag — no changes since {}", tag));
                println!("new_tag={}", tag);
                println!("old_tag={}", tag);
                println!("part=none");
                return Ok(());
            }
            log.verbose(&format!(
                "no changes since {}, but force_without_changes is enabled",
                tag
            ));
        }
    }

    // Scan commit messages to determine bump.  When a crate path is set,
    // only consider commits that actually touched that directory.
    let messages = get_messages_for_bump(
        &workspace_root_path,
        &cfg,
        prev_tag.as_deref(),
        crate_path.as_deref(),
    )?;
    log.verbose(&format!("scanned {} commit message(s)", messages.len()));

    // Detect bump (with pre-major demotion applied to inferred bumps).
    let bump = detect_bump_demoted(&messages, &cfg, prev_tag.as_deref());
    log.verbose(&format!("detected bump {:?}", bump));

    // The current manifest version for this tagging unit: the workspace
    // `[workspace.package].version` in lockstep mode, else the version-synced
    // crate's own `Cargo.toml`. Read+parsed once here and reused by both the
    // `cargo_ahead` release-signal check and the downgrade guard below so the
    // two never drift on which manifest they consult.
    let cargo_current_ver: Option<String> = if let Some(ws) = workspace_info {
        ws.workspace_package_version.clone()
    } else if version_sync_enabled && let Some(ref path) = crate_path {
        // Resolve against the discovered workspace root so the manifest read
        // matches the git working dir when `tag` runs from a subdirectory.
        let abs = workspace_root_path.join(path);
        anodizer_stage_build::version_sync::read_cargo_version(&abs.to_string_lossy()).ok()
    } else {
        None
    };

    // A manually-bumped Cargo.toml that is strictly ahead of the previous
    // tag is itself a release signal — the operator has explicitly set the
    // next version. Honor it even when no per-commit bump signal fired and
    // even when the crate path had no changes. This prevents autotag from
    // stalling at the old tag after a manual `cargo set-version` bump.
    let cargo_ahead = manifest_version_ahead(
        cargo_current_ver.as_deref(),
        prev_tag
            .as_deref()
            .and_then(|t| git::parse_semver_tag(t).ok())
            .map(|p| (p.major, p.minor, p.patch)),
    );

    // If #none token detected (and Cargo.toml isn't explicitly ahead), skip.
    // An explicit `--version` is itself the release signal, so it tags
    // regardless of any per-commit bump directive.
    if bump == BumpKind::None && !cargo_ahead && version_override.is_none() {
        log.verbose("skipped tag — no bump signal and Cargo.toml not ahead");
        println!("new_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("part=none");
        return Ok(());
    }

    // Determine base version.
    // When there is no previous tag, use initial_version directly without bumping
    // (matching github-tag-action behavior: initial_version IS the first tag).
    let (new_major, new_minor, new_patch, old_tag_str) = if let Some(ref prev) = prev_tag {
        let base = git::parse_semver_tag(prev)?;
        let (maj, min, pat) = apply_bump(base.major, base.minor, base.patch, &bump);
        (maj, min, pat, prev.as_str())
    } else {
        let base = git::parse_semver_tag(&format!("{}{}", cfg.tag_prefix, cfg.initial_version))
            .unwrap_or(git::SemVer {
                major: 0,
                minor: 1,
                patch: 0,
                prerelease: None,
                build_metadata: None,
            });
        (base.major, base.minor, base.patch, "")
    };

    // Build new version string
    let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);

    // Handle prerelease
    if cfg.prerelease {
        new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
    }

    // When version_sync is enabled, a Cargo.toml version already higher than
    // the tag-derived version wins, to avoid downgrading a manual bump. This is
    // the Cargo.toml-ahead guard; computing it here (even with `--version` set)
    // yields the version autotag *would* have produced, so the override warning
    // can name the true derived value the operator is overriding.
    if let Some(cargo_ver) = cargo_current_ver
        && manifest_version_ahead(Some(&cargo_ver), Some((new_major, new_minor, new_patch)))
    {
        if version_override.is_none() {
            log.status(&format!(
                "Cargo.toml version {} > tag-derived {}, using Cargo.toml version",
                cargo_ver, new_version
            ));
        }
        new_version = cargo_ver;
    }

    if let Some(pinned) = version_override {
        // The operator is authoritative: pin the explicit version verbatim,
        // bypassing the autotag bump AND the Cargo.toml-ahead guard above. Warn
        // when it disagrees with the version derivation would have produced
        // (`new_version` now holds that fully-derived value) so the divergence
        // is visible, then proceed with the explicit one.
        if pinned != new_version {
            log.warn(&format!(
                "--version {} overrides the derived version {} (autotag + Cargo.toml-ahead guard bypassed)",
                pinned, new_version
            ));
        }
        new_version = pinned;
    }

    let new_tag = format!("{}{}", cfg.tag_prefix, new_version);

    log.verbose(&format!("{} → {}", old_tag_str, new_tag));

    // When version_sync is enabled for this crate, update the Cargo.toml
    // version and commit before tagging so the tagged commit has the correct
    // version embedded.  This ensures cargo publish reads the right version.
    //
    // Also update intra-workspace dependency version specs so that other
    // crates referencing this one via path+version don't break.
    //
    // `[skip ci]` is opt-in via `tag.skip_ci_on_bump` (default off). It is NOT
    // a free CI-cost saving: the bump commit becomes the tag target, and a
    // `[skip ci]` tag target suppresses BOTH the master-push CI re-run AND any
    // `on: push: tags:` release trigger. It is only safe with a
    // `workflow_run`-triggered release; the tag-push pattern
    // must leave it off or the release silently never fires.
    if let Some(ws) = workspace_info {
        let root = workspace_root_path.as_path();
        // Lockstep shares one version across the whole workspace, so the
        // top-level `Config.version_files` list (no single crate to scope to)
        // is the enrollment, rewritten with the shared old→new.
        let ws_version_files = resolve_version_files(None, Some(&loaded_config));
        let ws_old = git::version_from_tag(old_tag_str);
        let ws_from_tag = (!old_tag_str.is_empty()).then_some(old_tag_str);
        let cl_config = changelog_config_for(Some(&loaded_config));
        let cl_routing = ChangelogRouting::from_config(&cl_config);
        apply_workspace_bump(
            root,
            ws,
            &new_version,
            &WorkspaceBumpEdits {
                vf: VersionFilesBump {
                    old: ws_old.as_deref(),
                    files: &ws_version_files,
                },
                cl: ChangelogBump {
                    enabled: changelog_enabled,
                    from_tag: ws_from_tag,
                    full_tag: &new_tag,
                    routing: &cl_routing,
                },
            },
            opts.dry_run,
            cfg.skip_ci_on_bump,
            &log,
        )?;
    } else if let Some(ref path) = crate_path
        && version_sync_enabled
    {
        // `path` is the config-declared (repo-root-relative) crate directory.
        // Resolve it against the discovered workspace root so the manifest /
        // dep-scan file IO hits the same tree git operates on even when `tag`
        // is invoked from a subdirectory.
        let abs_crate_dir = workspace_root_path
            .join(path)
            .to_string_lossy()
            .into_owned();
        anodizer_stage_build::version_sync::sync_version(
            &abs_crate_dir,
            &new_version,
            opts.dry_run,
            &log,
        )?;

        // Cross-crate dep updates scan from the discovered workspace root.
        let workspace_root = workspace_root_path.to_string_lossy().to_string();

        // Read the crate name from its Cargo.toml for dep scanning.
        let crate_cargo = std::path::Path::new(&abs_crate_dir).join("Cargo.toml");
        let crate_name = if let Ok(content) = std::fs::read_to_string(&crate_cargo) {
            content
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|doc| {
                    doc.get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
        } else {
            None
        };

        // Update dependency version specs in other crates that belong to the
        // SAME Cargo workspace as the bumped crate. Scoping to the owning
        // workspace prevents this bump from rewriting a path-dep pin in an
        // independent release group on a different cadence.
        let dep_modified = if let Some(ref name) = crate_name {
            anodizer_stage_build::version_sync::sync_workspace_deps(
                &workspace_root,
                &abs_crate_dir,
                name,
                &new_version,
                opts.dry_run,
                &log,
            )?
        } else {
            vec![]
        };

        // Rewrite enrolled version_files in the same bump commit so a Helm
        // Chart.yaml / install doc / README badge never drifts from the tag.
        // Old version comes from the previous tag; absent a previous tag there
        // is nothing to rewrite from. Runs in BOTH dry-run and real modes — the
        // helper logs per-file replacement counts (and the zero-match warning)
        // either way, and under dry-run writes/stages nothing — so the preview
        // matches the lockstep and per-crate paths.
        let vf_old = git::version_from_tag(old_tag_str);
        let vf_changed = match vf_old {
            Some(ref old) => rewrite_and_stage_version_files(
                &workspace_root_path,
                &crate_version_files,
                old,
                &new_version,
                opts.dry_run,
                &log,
            )?,
            None => Vec::new(),
        };

        // Refresh CHANGELOG.md alongside the version_files rewrites, on the same
        // dry-run-preview / real-write-and-stage split. The previous tag bounds
        // the rendered commit range (`old_tag_str` is empty on a first tag).
        let ws_root = Path::new(&workspace_root);
        let mut cl_markers: Vec<String> = Vec::new();
        let cl_changed = if changelog_enabled {
            let from_tag = (!old_tag_str.is_empty()).then(|| old_tag_str.to_string());
            let targets = crate_name
                .as_ref()
                .map(|name| {
                    vec![ChangelogTarget {
                        crate_name: name.clone(),
                        crate_dir: ws_root.join(path),
                        from_tag,
                        to_version: new_version.clone(),
                        full_tag: new_tag.clone(),
                    }]
                })
                .unwrap_or_default();
            let cl_config = changelog_config_for(Some(&loaded_config));
            let mut routing = ChangelogRouting::from_config(&cl_config);
            // `--crate <name>` single-target on a PerCrate workspace: topology
            // count is 1, so the renderer relies on the crate-name-aware
            // fallback. Supply the FULL root-routed crate set so an existing
            // `### <crate>` subsection is detected and a foreign heading is not.
            routing.root_crate_names = crate::commands::changelog_sync::config_root_crate_names(
                &loaded_config,
                routing.root_crates,
            );
            let changed =
                render_and_stage_changelogs(ws_root, &targets, &routing, opts.dry_run, &log)?;
            // Provenance markers for the bump commit body, derived from the
            // paths actually written: a crate earns a marker only when its
            // OWN crate-root CHANGELOG.md was regenerated, so the publish
            // guard never forgives drift in a file the tool did not touch.
            let marker_crates: Vec<(String, PathBuf, String)> = targets
                .iter()
                .map(|t| {
                    (
                        t.crate_name.clone(),
                        t.crate_dir.clone(),
                        t.to_version.clone(),
                    )
                })
                .collect();
            cl_markers = crate::commands::changelog_sync::changelog_provenance_markers(
                ws_root,
                &marker_crates,
                &changed,
            );
            changed
        } else {
            Vec::new()
        };

        if !opts.dry_run {
            // Regenerate Cargo.lock to match the bumped Cargo.toml versions.
            // Without this, the tagged commit has Cargo.toml at the new version
            // but Cargo.lock at the old version, causing `cargo test` (from
            // before hooks) to update Cargo.lock and dirty the tree.
            match anodizer_core::cargo_lock::cargo_update_workspace(Some(
                workspace_root_path.as_path(),
            )) {
                Ok(true) => {}
                Ok(false) => warn_cargo_lock_stale(
                    &log,
                    "`cargo update --workspace` exited non-zero after version sync",
                ),
                Err(e) => warn_cargo_lock_stale(
                    &log,
                    &format!("could not spawn `cargo update --workspace` ({e})"),
                ),
            }

            let cargo_toml = format!("{}/Cargo.toml", path);
            let mut files_to_stage: Vec<&str> = vec![&cargo_toml, "Cargo.lock"];
            for f in &dep_modified {
                files_to_stage.push(f);
            }
            for f in &vf_changed {
                files_to_stage.push(f);
            }
            for f in &cl_changed {
                if !files_to_stage.contains(&f.as_str()) {
                    files_to_stage.push(f);
                }
            }
            // Propagate a commit failure (index lock, hook rejection, …)
            // before any tag is created: tagging a commit whose Cargo.toml is
            // NOT at `new_version` would ship a tag pointing at the wrong
            // version. Staged from the discovered workspace root so the
            // repo-relative paths resolve there, not against a subdirectory
            // cwd.
            git::stage_and_commit_in(
                &workspace_root_path,
                &files_to_stage,
                &crate::commands::changelog_sync::commit_message_with_markers(
                    git::release_bump_subject(
                        &format!("{} → {}", path, new_version),
                        skip_ci_suffix(cfg.skip_ci_on_bump),
                    ),
                    &cl_markers,
                ),
            )?;
        }
    }

    // Create and push tag
    let prev_for_hook = if old_tag_str.is_empty() {
        None
    } else {
        Some(old_tag_str)
    };
    create_tag(
        &new_tag,
        &format!("Release {}", new_tag),
        opts.dry_run,
        prev_for_hook,
    )?;

    // The implicit default kept everything local; make that explicit so a
    // user expecting a published tag isn't surprised later. Stay silent when
    // the user explicitly chose --no-push / --push-tags-only (they picked a
    // push mode deliberately) or in any dry-run/preview mode (nothing was
    // created for real).
    if push_branch.is_none()
        && !opts.no_push
        && !opts.push_tags_only
        && !opts.dry_run
        && !opts.push_dry_run
    {
        log.status(&format!(
            "created {} locally; nothing was pushed — \
             pass --push to push the bump commit + tag atomically",
            new_tag
        ));
    }

    let part_str = match bump {
        BumpKind::Major => "major",
        BumpKind::Minor => "minor",
        BumpKind::Patch => "patch",
        BumpKind::None => "none",
    };

    println!("new_tag={}", new_tag);
    println!("old_tag={}", old_tag_str);
    println!("part={}", part_str);

    Ok(())
}
