//! Pull-request submission flows.
//!
//! Two public entry points:
//! - [`maybe_submit_pr`] — gated on `repo.pull_request.enabled`, used by
//!   the homebrew/scoop/winget/chocolatey/aur publishers.
//! - [`submit_pr_via_gh_with_opts`] — unconditional submission used by
//!   krew's legacy path and winget's `microsoft/winget-pkgs` fallback.
//!
//! Internally tries `gh` CLI first, falls back to the GitHub REST API,
//! and best-effort rebases the fork against upstream when the PR
//! crosses repos.

use anodizer_core::PublisherOutcome;
use anodizer_core::config::RepositoryConfig;
use anodizer_core::log::StageLogger;
use std::path::Path;
use std::process::Command;

use super::branch::fetch_default_branch;
use super::cmd::run_cmd_in;

/// Sync a fork with its upstream base repository.
///
/// When PR mode targets a different (upstream) repository, the fork may be
/// behind.  This fetches the upstream base branch and rebases local work on
/// top, mirroring GoReleaser's `ForkSyncer.SyncFork()` behaviour.
///
/// This is a best-effort operation: if the sync fails the push will still
/// proceed (the PR may simply have merge conflicts).
fn sync_fork(
    repo_path: &Path,
    upstream_url: &str,
    base_branch: &str,
    label: &str,
    log: &StageLogger,
) {
    // Add the upstream remote (ignore error if it already exists).
    let _ = run_cmd_in(
        repo_path,
        "git",
        &["remote", "add", "upstream", upstream_url],
        &format!("{label}: git remote add upstream"),
    );

    // Fetch the upstream base branch.
    if let Err(e) = run_cmd_in(
        repo_path,
        "git",
        &["fetch", "upstream", base_branch],
        &format!("{label}: git fetch upstream"),
    ) {
        log.warn(&format!(
            "{label}: fork sync: fetch upstream failed, continuing without sync: {e}"
        ));
        return;
    }

    // Rebase local work on top of the upstream base branch.
    let upstream_ref = format!("upstream/{}", base_branch);
    if let Err(e) = run_cmd_in(
        repo_path,
        "git",
        &["rebase", &upstream_ref],
        &format!("{label}: git rebase upstream"),
    ) {
        log.warn(&format!(
            "{label}: fork sync: rebase failed, aborting rebase and continuing: {e}"
        ));
        // Abort the failed rebase so the repo is in a clean state.
        let _ = run_cmd_in(
            repo_path,
            "git",
            &["rebase", "--abort"],
            &format!("{label}: git rebase --abort"),
        );
    }
}

/// Check whether the `gh` CLI is available in PATH.
fn gh_is_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Shared messages — single source of truth for the PR-already-exists
// branch so both transports and the unit tests assert on the same
// string. Mirrors the `run_*_message` pattern used elsewhere in
// stage-publish.
// ---------------------------------------------------------------------------

/// Warn message rendered when the gh CLI / API reports the PR already
/// exists and `update_existing_pr` is false. Operators see this in
/// the publish log and the actionable remediation pointer at the end.
pub(crate) fn pr_exists_skip_warn_message(label: &str, head: &str) -> String {
    format!(
        "{label}: PR for '{head}' already exists — skipping \
         (set update_existing_pr: true to update the PR in place)"
    )
}

/// Status message rendered when the gh CLI reports the PR already
/// exists, `update_existing_pr` is true, and the force-push to the
/// existing branch succeeded.
pub(crate) fn pr_exists_update_status_message(label: &str, head: &str) -> String {
    format!("{label}: PR for '{head}' already exists — updated in place")
}

// ---------------------------------------------------------------------------
// PR specs — bundle the request shape shared by both transports
// ---------------------------------------------------------------------------

/// Pull-request payload, shared by [`create_pr_via_gh_cli`] and
/// [`create_pr_via_api`].
///
/// All fields are borrowed; the struct is short-lived and lives on the
/// caller's stack frame.
#[derive(Clone, Copy)]
pub(crate) struct PrSpec<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub head: &'a str,
    pub base_branch: &'a str,
    pub draft: bool,
    /// When true, force-push the branch to update an existing PR in place
    /// rather than skipping when `gh pr create` reports "already exists".
    pub update_existing_pr: bool,
}

/// Upstream repository identity (owner + name).
///
/// Used by the API transport (which builds
/// `https://api.github.com/repos/{owner}/{name}/pulls`) and by
/// [`maybe_submit_pr`] when resolving a configured PR base.
#[derive(Clone, Copy)]
pub(crate) struct Upstream<'a> {
    pub owner: &'a str,
    pub name: &'a str,
}

/// Submit a pull request via the GitHub CLI (`gh pr create`).
///
/// Returns `Some(PublisherOutcome::PendingValidation)` when the call hit
/// the "PR already exists" branch and `update_existing_pr` was false, or
/// `Some(Failed(msg))` when the PR could not be created (transport
/// failure, retry budget exhausted, non-success exit). Returns `None`
/// on success, which includes both the newly-created-PR path AND the
/// `update_existing_pr=true` force-push branch (existing PR was updated
/// in place — also a success outcome). The caller threads the outcome
/// back to dispatch via `Context::record_publisher_outcome` so the
/// summary table reads the real terminal state instead of misreporting
/// silent failures as `succeeded`.
fn create_pr_via_gh_cli(
    repo_path: &Path,
    upstream_repo: &str,
    spec: &PrSpec<'_>,
    label: &str,
    log: &StageLogger,
) -> Option<PublisherOutcome> {
    let PrSpec {
        title,
        body,
        head,
        base_branch,
        draft,
        update_existing_pr,
    } = *spec;
    // `head` is "owner:branch"; extract just the branch name for push.
    let branch_name = head.split_once(':').map(|(_, b)| b).unwrap_or(head);
    let mut args = vec![
        "pr",
        "create",
        "--repo",
        upstream_repo,
        "--title",
        title,
        "--body",
        body,
        "--head",
        head,
        "--base",
        base_branch,
    ];
    if draft {
        args.push("--draft");
    }
    // GitHub's API occasionally lags behind a just-pushed fork branch, so the
    // first `gh pr create` can fail with "No commits between ..." or "Head sha
    // can't be blank" even though the push succeeded. These are transient and
    // resolve within a few seconds. Retry up to 3 times with short backoffs
    // before warning.
    let mut last_stderr = String::new();
    let mut last_status: Option<std::process::ExitStatus> = None;
    for attempt in 1..=3 {
        let pr_result = Command::new("gh")
            .current_dir(repo_path)
            .args(&args)
            .output();
        match pr_result {
            Ok(output) if output.status.success() => {
                log.status(&format!("{label}: PR submitted via gh CLI"));
                return None;
            }
            Ok(output) => {
                last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
                last_status = Some(output.status);
                // An open PR with identical head/base already exists.
                if last_stderr.contains("already exists") {
                    if update_existing_pr {
                        // Force-push to the existing branch so the open PR
                        // picks up the new manifest without needing a new PR.
                        if let Err(e) = run_cmd_in(
                            repo_path,
                            "git",
                            &["push", "--force-with-lease", "origin", branch_name],
                            &format!("{label}: git push --force-with-lease (update existing PR)"),
                        ) {
                            log.warn(&format!(
                                "{label}: update_existing_pr=true but force-push failed: {e}"
                            ));
                        } else {
                            log.status(&pr_exists_update_status_message(label, head));
                        }
                        return None;
                    } else {
                        log.warn(&pr_exists_skip_warn_message(label, head));
                        return Some(PublisherOutcome::PendingValidation);
                    }
                }
                let transient = last_stderr.contains("No commits between")
                    || last_stderr.contains("Head sha can't be blank")
                    || last_stderr.contains("Head repository can't be blank")
                    || last_stderr.contains("not all refs are readable");
                if !transient || attempt == 3 {
                    break;
                }
                log.warn(&format!(
                    "{label}: gh pr create attempt {attempt}/3 hit transient error; retrying..."
                ));
                std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
            }
            Err(e) => {
                let msg = format!(
                    "{label}: could not run gh to create PR: {e} -- you may need to create the PR manually"
                );
                log.warn(&msg);
                // Silent-fail would let dispatch record Succeeded.
                // Return Failed so the report tells the truth;
                // non-required publishers won't gate the release.
                return Some(PublisherOutcome::Failed(msg));
            }
        }
    }
    let msg = format!(
        "{label}: gh pr create exited with {} -- you may need to create the PR manually{}",
        last_status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown status".to_string()),
        if last_stderr.is_empty() {
            String::new()
        } else {
            format!("\n{}", last_stderr)
        }
    );
    log.warn(&msg);
    Some(PublisherOutcome::Failed(msg))
}

/// Submit a pull request via the GitHub REST API (native fallback when `gh`
/// CLI is not installed).
///
/// Uses `POST /repos/{owner}/{repo}/pulls` with token-based auth.
///
/// Returns `Some(PublisherOutcome::PendingValidation)` when the API
/// returns 422 with a body that names the existing-PR case. This holds
/// whether or not the caller opted into `update_existing_pr`: the API
/// transport cannot force-push (no working tree handy), so
/// `update_existing_pr = true` is a no-op here — we warn that the
/// in-place update needs `gh` CLI but still surface `PendingValidation`
/// because the open PR did not advance to the new manifest. Returns
/// `None` on success. Returns `Some(Failed(msg))` for transport
/// failure, HTTP-client build failure, and non-success HTTP status —
/// silent-fail would let dispatch record `succeeded`.
fn create_pr_via_api(
    upstream: &Upstream<'_>,
    spec: &PrSpec<'_>,
    token: &str,
    label: &str,
    log: &StageLogger,
) -> Option<PublisherOutcome> {
    let Upstream { owner, name } = *upstream;
    let PrSpec {
        title,
        body,
        head,
        base_branch,
        draft,
        update_existing_pr,
    } = *spec;
    let url = format!("https://api.github.com/repos/{}/{}/pulls", owner, name);
    let payload = serde_json::json!({
        "title": title, "head": head, "base": base_branch, "body": body, "draft": draft,
    });
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{label}: build HTTP client: {e}");
            log.warn(&msg);
            // Silent-fail = dispatch records Succeeded; return Failed instead.
            return Some(PublisherOutcome::Failed(msg));
        }
    };
    let result = client
        .post(&url)
        .header("Authorization", format!("token {}", token))
        .header("Accept", "application/vnd.github+json")
        .json(&payload)
        .send();
    match result {
        Ok(resp) if resp.status().is_success() => {
            log.status(&format!("{label}: PR submitted via GitHub API"));
            None
        }
        Ok(resp) => {
            let status = resp.status();
            let body_text = anodizer_core::http::body_of_blocking(resp);
            // GitHub returns 422 Unprocessable Entity with a body that
            // mentions "A pull request already exists" when head/base
            // collide. Treat that as PendingValidation so the summary
            // table tells the truth — `succeeded` would be a lie.
            if status.as_u16() == 422 && body_text.contains("already exists") {
                if update_existing_pr {
                    log.warn(&format!(
                        "{label}: PR for '{head}' already exists and update_existing_pr=true \
                         was requested, but the API transport cannot force-push; \
                         install `gh` CLI to update the PR in place"
                    ));
                } else {
                    log.warn(&pr_exists_skip_warn_message(label, head));
                }
                return Some(PublisherOutcome::PendingValidation);
            }
            let msg = format!(
                "{label}: GitHub API PR creation returned {status} -- you may need to create the PR manually\n{body_text}"
            );
            log.warn(&msg);
            Some(PublisherOutcome::Failed(msg))
        }
        Err(e) => {
            let msg = format!(
                "{label}: GitHub API PR creation failed: {e} -- you may need to create the PR manually"
            );
            log.warn(&msg);
            Some(PublisherOutcome::Failed(msg))
        }
    }
}

/// Origin (the fork) coordinates passed to [`maybe_submit_pr`].
#[derive(Clone, Copy)]
pub(crate) struct PrOrigin<'a> {
    pub repo_owner: &'a str,
    pub repo_name: &'a str,
    pub branch_name: &'a str,
    /// When true, force-push to an existing PR branch rather than skipping.
    pub update_existing_pr: bool,
}

/// Submit a pull request if `repo.pull_request.enabled` is true.
///
/// Uses `pull_request.base` for the upstream target when available,
/// falling back to `origin.repo_owner/origin.repo_name`. Supports
/// `pull_request.draft`.
///
/// When the base repository differs from the fork (i.e. a PR across repos),
/// the fork is synced with upstream before submitting (GoReleaser parity).
///
/// Tries `gh` CLI first; if unavailable, falls back to the GitHub REST API
/// using the token from the RepositoryConfig (or `GITHUB_TOKEN` env var).
///
/// Returns `Some(PublisherOutcome::PendingValidation)` when the PR
/// already exists and could not be updated, or `Some(Failed(msg))` when
/// PR creation failed (gh / token absent, transport error, exhausted
/// retries, non-success HTTP status). Callers MUST forward the value to
/// `Context::record_publisher_outcome` or the dispatch summary table
/// will misreport the silent failure as `succeeded`.
#[must_use = "the returned outcome override must be forwarded to \
              Context::record_publisher_outcome — dropping it silently \
              misreports a PR-already-exists skip or a PR-creation failure \
              as `succeeded`"]
pub(crate) fn maybe_submit_pr(
    repo_path: &Path,
    repo: Option<&RepositoryConfig>,
    origin: &PrOrigin<'_>,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) -> Option<PublisherOutcome> {
    let PrOrigin {
        repo_owner,
        repo_name,
        branch_name,
        update_existing_pr,
    } = *origin;
    let pr_cfg = match repo.and_then(|r| r.pull_request.as_ref()) {
        Some(pr) if pr.enabled == Some(true) => pr,
        _ => return None,
    };
    let (upstream_owner, upstream_name) = if let Some(ref base) = pr_cfg.base {
        (
            base.owner.as_deref().unwrap_or(repo_owner),
            base.name.as_deref().unwrap_or(repo_name),
        )
    } else {
        (repo_owner, repo_name)
    };
    let upstream_slug = format!("{}/{}", upstream_owner, upstream_name);
    let pr_body = pr_cfg.body.as_deref().unwrap_or(body);
    let head = format!("{}:{}", repo_owner, branch_name);
    let is_draft = pr_cfg.draft == Some(true);
    let base_branch = pr_cfg
        .base
        .as_ref()
        .and_then(|b| b.branch.as_deref())
        .unwrap_or("main");
    let token = repo
        .and_then(|r| r.token.clone())
        .or_else(|| std::env::var("ANODIZER_GITHUB_TOKEN").ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    // Fork sync: when the PR targets a different upstream repository, sync first.
    let is_cross_repo = upstream_owner != repo_owner || upstream_name != repo_name;
    if is_cross_repo {
        let upstream_url = format!(
            "https://github.com/{}/{}.git",
            upstream_owner, upstream_name
        );
        sync_fork(repo_path, &upstream_url, base_branch, label, log);
        if let Err(e) = run_cmd_in(
            repo_path,
            "git",
            &["push", "--force-with-lease", "origin", branch_name],
            &format!("{label}: git push (post-sync)"),
        ) {
            log.warn(&format!(
                "{label}: fork sync: force-push after rebase failed, PR may have conflicts: {e}"
            ));
        }
    }

    let spec = PrSpec {
        title,
        body: pr_body,
        head: &head,
        base_branch,
        draft: is_draft,
        update_existing_pr,
    };
    let upstream = Upstream {
        owner: upstream_owner,
        name: upstream_name,
    };

    // PR creation: try gh CLI first, fall back to GitHub API.
    if gh_is_available() {
        create_pr_via_gh_cli(repo_path, &upstream_slug, &spec, label, log)
    } else if let Some(ref tok) = token {
        create_pr_via_api(&upstream, &spec, tok, label, log)
    } else {
        let msg = format!(
            "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
        );
        log.warn(&msg);
        // Silent-fail = dispatch records Succeeded (v0.3.0 lie shape).
        Some(PublisherOutcome::Failed(msg))
    }
}

/// Options for [`submit_pr_via_gh_with_opts`]. Bundles infrequently-varying
/// knobs so the function stays within the argument-count lint budget.
#[derive(Clone, Copy, Default)]
pub(crate) struct SubmitPrOpts {
    /// When true, force-push to an existing PR branch rather than skipping.
    pub update_existing_pr: bool,
}

/// Submit a pull request via the GitHub CLI. Logs a warning instead of failing
/// if `gh` is not available or the command exits non-zero.
///
/// Supports `opts.update_existing_pr` to force-push to an existing PR branch
/// rather than skipping when a PR already exists.
///
/// Falls back to the GitHub REST API when `gh` is unavailable and a token
/// can be resolved from the environment.
///
/// Returns `Some(PublisherOutcome::PendingValidation)` when the PR
/// already exists and could not be updated, or `Some(Failed(msg))` when
/// PR creation failed (gh / token absent, transport error, exhausted
/// retries, non-success HTTP status). Callers MUST forward the value to
/// `Context::record_publisher_outcome` or the dispatch summary table
/// will misreport the silent failure as `succeeded`.
#[allow(clippy::too_many_arguments)]
#[must_use = "the returned outcome override must be forwarded to \
              Context::record_publisher_outcome — dropping it silently \
              misreports a PR-already-exists skip or a PR-creation failure \
              as `succeeded`"]
pub(crate) fn submit_pr_via_gh_with_opts(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
    opts: SubmitPrOpts,
) -> Option<PublisherOutcome> {
    let token = std::env::var("ANODIZER_GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    // Discover the upstream's actual default branch. Hardcoding "main" breaks
    // PR creation against repos whose default is "master" (e.g.
    // microsoft/winget-pkgs) or any other name. The 404 on the base ref
    // bubbles up as a tangled GraphQL error from `gh pr create`:
    // "Head sha can't be blank, ..., not all refs are readable, Base ref
    // must be a branch". Fall back to "main" only if the lookup fails.
    let base_branch = upstream_repo
        .split_once('/')
        .and_then(|(owner, name)| fetch_default_branch(owner, name, token.as_deref()))
        .unwrap_or_else(|| "main".to_string());

    let spec = PrSpec {
        title,
        body,
        head,
        base_branch: &base_branch,
        draft: false,
        update_existing_pr: opts.update_existing_pr,
    };

    if gh_is_available() {
        create_pr_via_gh_cli(repo_path, upstream_repo, &spec, label, log)
    } else if let Some(ref tok) = token {
        if let Some((owner, name)) = upstream_repo.split_once('/') {
            let upstream = Upstream { owner, name };
            create_pr_via_api(&upstream, &spec, tok, label, log)
        } else {
            let msg = format!(
                "{label}: cannot parse upstream repo slug '{upstream_repo}' for API fallback"
            );
            log.warn(&msg);
            Some(PublisherOutcome::Failed(msg))
        }
    } else {
        let msg = format!(
            "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
        );
        log.warn(&msg);
        // Silent-fail = dispatch records Succeeded (v0.3.0 lie shape).
        Some(PublisherOutcome::Failed(msg))
    }
}

#[cfg(test)]
mod tests {
    use anodizer_core::config::{HomebrewCaskConfig, KrewConfig, StringOrBool, WingetConfig};

    /// Config field roundtrip: `update_existing_pr` on WingetConfig survives serde.
    #[test]
    fn winget_update_existing_pr_bool_roundtrips() {
        let cfg = WingetConfig {
            update_existing_pr: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: WingetConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.update_existing_pr,
            Some(StringOrBool::Bool(true))
        ));
    }

    /// Config field roundtrip: `update_existing_pr` absent defaults to None.
    #[test]
    fn winget_update_existing_pr_absent_is_none() {
        let cfg: WingetConfig = serde_json::from_str("{}").expect("deserialize");
        assert!(cfg.update_existing_pr.is_none());
    }

    /// Config field roundtrip: `update_existing_pr` on KrewConfig survives serde.
    #[test]
    fn krew_update_existing_pr_bool_roundtrips() {
        let cfg = KrewConfig {
            update_existing_pr: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: KrewConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.update_existing_pr,
            Some(StringOrBool::Bool(true))
        ));
    }

    /// Config field roundtrip: `update_existing_pr` on HomebrewCaskConfig survives serde.
    #[test]
    fn homebrew_cask_update_existing_pr_bool_roundtrips() {
        let cfg = HomebrewCaskConfig {
            update_existing_pr: Some(StringOrBool::Bool(false)),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: HomebrewCaskConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.update_existing_pr,
            Some(StringOrBool::Bool(false))
        ));
    }

    /// Skip warn message contains guidance when update_existing_pr=false.
    #[test]
    fn pr_exists_skip_warn_contains_guidance() {
        let msg = super::pr_exists_skip_warn_message("winget", "owner:my-app-1.2.3");
        assert!(msg.contains("already exists"), "{msg}");
        assert!(msg.contains("update_existing_pr: true"), "{msg}");
        assert!(msg.contains("winget"), "{msg}");
        assert!(msg.contains("owner:my-app-1.2.3"), "{msg}");
    }

    /// Update-in-place status message contains correct indicator.
    #[test]
    fn pr_exists_update_status_contains_updated_in_place() {
        let msg = super::pr_exists_update_status_message("winget", "owner:my-app-1.2.3");
        assert!(msg.contains("updated in place"), "{msg}");
        assert!(msg.contains("winget"), "{msg}");
        assert!(msg.contains("owner:my-app-1.2.3"), "{msg}");
    }
}
