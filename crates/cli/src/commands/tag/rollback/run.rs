use super::deletion::{delete_tags, resolve_push_branch};
use super::guard::{
    BurnProbes, WingetProbeSpec, check_not_irreversibly_published, winget_probe_token,
};
use super::tags::{
    ANODIZE_REVERT_SUBJECT_PREFIX, build_revert_message, classify_tag, rollback_subject_prefix,
    scope_includes,
};
use super::types::{Mode, RollbackOpts};
use super::{first_line, short};
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::{Result, bail};

pub fn run(opts: RollbackOpts) -> Result<()> {
    run_with_gh(opts, std::path::Path::new("gh"))
}

/// Path-taking sibling of [`run`]: `gh_binary` is the `gh` CLI used by
/// the published-state guard's GitHub-release fallback probe.
/// Production passes `Path::new("gh")` (PATH lookup); tests point at a
/// stub script so no global PATH mutation is needed (same seam
/// convention as `core::git::gh_api_get_with_binary`).
pub(super) fn run_with_gh(opts: RollbackOpts, gh_binary: &std::path::Path) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let log = StageLogger::new(
        "tag-rollback",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let raw_target = opts.sha.as_deref().unwrap_or("HEAD");
    let target_sha = git::rev_parse_in(&cwd, raw_target)?;
    log.kv(
        "target",
        &format!("{} ({})", raw_target, short(&target_sha)),
        "target".len(),
    );

    let all_tags_at_sha = git::get_tags_at_sha_in(&cwd, &target_sha)?;
    if all_tags_at_sha.is_empty() {
        log.warn(&format!("no tags found at {}", short(&target_sha)));
        bail!(
            "refusing to roll back: no tags point at {} — pass the bumped commit's SHA explicitly",
            short(&target_sha)
        );
    }

    let mut deletable: Vec<String> = Vec::new();
    for tag in &all_tags_at_sha {
        match classify_tag(tag) {
            None => log.status(&format!("skipped {tag} (not anodize-shaped)")),
            Some(kind) if !scope_includes(opts.scope, kind) => log.status(&format!(
                "skipped {tag} (scope filter --scope={:?})",
                opts.scope
            )),
            Some(_) => deletable.push(tag.clone()),
        }
    }

    if deletable.is_empty() {
        log.warn(&format!(
            "no anodize-managed tags at {} match --scope={:?}",
            short(&target_sha),
            opts.scope
        ));
        return Ok(());
    }

    // Published-state guard, BEFORE any mutation (including dry-run,
    // so the preview reports the same refusal the real run would).
    // A one-way-door (Submitter) publisher that landed for one of these
    // tags burned the version: registries like crates.io / chocolatey /
    // winget / snapcraft never accept the same version twice, so
    // deleting the tag + reverting the bump can never lead to a clean
    // same-version re-cut — only to an orphaned live release.
    // Tags whose GitHub release this rollback owns (a run summary attributes
    // them to the attempt being rolled back, or --force overrode the guard).
    // Only these get their release deleted; an unattributed tag's release is
    // preserved (it may be a human's draft or a prior reversible release).
    let attributed: std::collections::HashSet<String> = if opts.force {
        log.warn("skipped the published-state guard — --force");
        deletable.iter().cloned().collect()
    } else {
        // Fail-closed config load: the config drives the dist-dir resolution
        // for run summaries and the tag→crate mapping for the crates.io index
        // probe. A missing or unparseable config would blind the probe — the
        // exact failure mode the guard exists to prevent — so it refuses
        // instead of silently narrowing the evidence (a network error already
        // refuses; a config error must not be weaker). The probe itself
        // reuses the publish stage's sparse-index client so rollback and
        // publish can never disagree about what "published on crates.io"
        // means.
        let repo_config = match crate::pipeline::load_repo_config(&cwd) {
            Ok(config) => config,
            Err(e) => bail!(
                "refusing to roll back — could not load the anodizer config: {e:#}\n\
                 The published-state guard needs the config to map the tag(s) to crates \
                 for the crates.io burn probe; without that mapping there is no proof the \
                 version(s) are safe to destroy — a prior run may have burned them on a \
                 one-way-door registry. Fix the config, or run from a checkout whose \
                 config parses (e.g. the directory that contains it). As a last resort, \
                 --force skips ALL published-state checks (run summaries, crates.io, \
                 GitHub releases), not just this config probe — use it only if you are \
                 certain nothing irreversible shipped."
            ),
        };
        // Guard probes get their own shallow retry ladder (GUARD_PROBE:
        // 3 attempts, 30s cap) instead of the run's configured publish
        // ladder: a multi-crate workspace probes many registry endpoints in
        // one pass, and a registry outage must fail the guard closed in
        // seconds-to-minutes, not burn a full ~25-minute backoff budget per
        // crate first.
        let probe_policy = anodizer_core::retry::RetryPolicy::GUARD_PROBE;
        let index_probe = |name: &str, version: &str| {
            anodizer_stage_publish::cargo::published_on_crates_io(
                name,
                version,
                &probe_policy,
                &log,
            )
        };
        let npm_probe = |registry: &str, package: &str, version: &str| {
            anodizer_stage_publish::npm::version_visible_on_registry(
                registry,
                package,
                version,
                &probe_policy,
                &log,
            )
        };
        let pypi_probe = |repository: &str, project: &str, version: &str| {
            anodizer_stage_publish::pypi::pypi_version_live(
                repository,
                project,
                version,
                &probe_policy,
                &log,
            )
        };
        let winget_token = winget_probe_token(&anodizer_core::ProcessEnvSource);
        let choco_probe = |package: &str, version: &str| {
            anodizer_stage_publish::post_publish::chocolatey::version_blocked_on_gallery(
                "https://community.chocolatey.org",
                package,
                version,
                &log,
            )
        };
        let winget_probe = |spec: &WingetProbeSpec| {
            anodizer_stage_publish::post_publish::winget::version_pr_blocking(
                "https://api.github.com",
                &spec.upstream,
                &spec.package_id,
                &spec.version,
                spec.search_in_title,
                winget_token.as_deref(),
                &log,
            )
        };
        let probes = BurnProbes {
            crates_io: &index_probe,
            npm: &npm_probe,
            pypi: &pypi_probe,
            chocolatey: &choco_probe,
            winget: &winget_probe,
        };
        let unsummarized = check_not_irreversibly_published(
            &cwd,
            gh_binary,
            &deletable,
            &repo_config,
            &probes,
            &log,
        )?;
        deletable
            .iter()
            .filter(|t| !unsummarized.contains(t))
            .cloned()
            .collect()
    };

    // Safety check (--mode=revert only). Non-bump commits on top of
    // the target SHA mean someone landed unrelated work since the
    // bump; reverting blindly would lose it. Tolerate only anodize's
    // OWN prior revert commit so re-runs are idempotent — a generic
    // `"Revert "<...>"` prefix would silently absorb GitHub's
    // "Revert this PR" button output (e.g. an unrelated feature
    // revert) and disable the safety net.
    if opts.mode == Mode::Revert {
        let intervening = git::commits_with_subjects_in(&cwd, &target_sha)?;
        let mut suspicious: Vec<(String, String)> = Vec::new();
        for (sha, subject) in &intervening {
            if subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str())
                || subject.starts_with(&rollback_subject_prefix())
            {
                continue;
            }
            suspicious.push((sha.clone(), subject.clone()));
        }
        if !suspicious.is_empty() {
            let mut msg = format!(
                "cannot rollback — {} non-bump commit(s) sit between HEAD and {}:\n",
                suspicious.len(),
                short(&target_sha)
            );
            for (sha, subj) in &suspicious {
                msg.push_str(&format!("  {} {}\n", short(sha), subj));
            }
            msg.push_str("resolve manually, or use --mode=reset to force.");
            bail!("{msg}");
        }
    }

    // Local mutation runs FIRST so a failed revert / reset leaves the
    // remote tags intact. Operator can retry without staring down a
    // half-rolled-back remote (tag gone) + intact local (tag still
    // present + bump commit still HEAD). Per-tag remote delete happens
    // after the local mutation succeeds — if a single remote-delete
    // glitches, the revert is already on disk and ready to push.

    // Mode=reset short-circuits revert+push entirely. Print a loud
    // warning so the operator knows they own the force-push.
    if opts.mode == Mode::Reset {
        let parent = format!("{}~1", target_sha);
        if opts.dry_run {
            log.status(&format!(
                "(dry-run) would run: git reset --hard {} (parent of bump commit)",
                short(&target_sha)
            ));
        } else {
            git::reset_hard_in(&cwd, &parent)?;
            log.status(&format!(
                "reset HEAD to {} (parent of bump commit)",
                short(&target_sha)
            ));
        }
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
        log.warn(
            "--mode=reset rewrote local history. Push with \
             `git push --force-with-lease origin <branch>` when ready.",
        );
        return Ok(());
    }

    // Mode=revert: create the revert commit, PUSH it, then delete tags.
    // Push precedes the remote tag delete so a push failure (e.g. a
    // non-fast-forward) leaves the tags intact and the rollback safely
    // retryable — never a tag-deleted-but-commit-unpushed limbo. The commit
    // message lists the tags that WILL be deleted (or under --dry-run, that
    // WOULD be deleted).
    let message = build_revert_message(&target_sha, &deletable, opts.dry_run);
    if opts.dry_run {
        log.status(&format!(
            "(dry-run) would run: git revert --no-edit {} && git commit --amend -m {:?}",
            short(&target_sha),
            message
        ));
    } else {
        let identity = git::resolve_rollback_identity(&cwd);
        git::revert_commit_in(&cwd, &target_sha, Some(&message), &identity)?;
        log.status(&format!("created revert commit {}", first_line(&message)));
    }

    if opts.no_push {
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
        log.status("skipped branch push — --no-push");
        return Ok(());
    }
    let branch = resolve_push_branch(&cwd, &target_sha, opts.branch.as_deref())?;
    if opts.dry_run {
        log.status(&format!("(dry-run) would run: git push origin {branch}"));
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
    } else {
        // Push BEFORE deleting remote tags: the destructive tag delete is the
        // last step, so a push failure aborts before any tag is dropped.
        git::push_branch_in(&cwd, &branch)?;
        log.status(&format!("pushed revert to origin/{branch}"));
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
    }
    Ok(())
}
