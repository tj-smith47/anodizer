//! Pull-request submission flows.
//!
//! Two public entry points:
//! - [`maybe_submit_pr`] — gated on `repo.pull_request.enabled`, used by
//!   the homebrew/scoop/winget/chocolatey/aur publishers.
//! - [`submit_pr_via_gh`] — unconditional submission used by krew.
//!
//! Internally tries `gh` CLI first, falls back to the GitHub REST API,
//! and best-effort rebases the fork against upstream when the PR
//! crosses repos.

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

/// Submit a pull request via the GitHub CLI (`gh pr create`).
#[allow(clippy::too_many_arguments)]
fn create_pr_via_gh_cli(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
    label: &str,
    log: &StageLogger,
) {
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
                return;
            }
            Ok(output) => {
                last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
                last_status = Some(output.status);
                // Idempotent success: an open PR with identical head/base
                // already exists. `gh` emits this after the fork was synced
                // by a prior publish attempt.
                if last_stderr.contains("already exists") {
                    log.status(&format!(
                        "{label}: PR for '{head}' already exists — skipping"
                    ));
                    return;
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
                log.warn(&format!(
                    "{label}: could not run gh to create PR: {} -- you may need to create the PR manually", e
                ));
                return;
            }
        }
    }
    log.warn(&format!(
        "{label}: gh pr create exited with {} -- you may need to create the PR manually{}",
        last_status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown status".to_string()),
        if last_stderr.is_empty() {
            String::new()
        } else {
            format!("\n{}", last_stderr)
        }
    ));
}

/// Submit a pull request via the GitHub REST API (native fallback when `gh`
/// CLI is not installed).
///
/// Uses `POST /repos/{owner}/{repo}/pulls` with token-based auth.
#[allow(clippy::too_many_arguments)]
fn create_pr_via_api(
    upstream_owner: &str,
    upstream_name: &str,
    head: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
    token: &str,
    label: &str,
    log: &StageLogger,
) {
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls",
        upstream_owner, upstream_name
    );
    let payload = serde_json::json!({
        "title": title, "head": head, "base": base_branch, "body": body, "draft": draft,
    });
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!("{label}: build HTTP client: {e}"));
            return;
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
        }
        Ok(resp) => {
            let status = resp.status();
            let body_text = resp.text().unwrap_or_default();
            log.warn(&format!(
                "{label}: GitHub API PR creation returned {status} -- you may need to create the PR manually\n{body_text}"
            ));
        }
        Err(e) => {
            log.warn(&format!(
                "{label}: GitHub API PR creation failed: {e} -- you may need to create the PR manually"
            ));
        }
    }
}

/// Submit a pull request if `repo.pull_request.enabled` is true.
///
/// Uses `pull_request.base` for the upstream target when available,
/// falling back to `repo_owner/repo_name`.  Supports `pull_request.draft`.
///
/// When the base repository differs from the fork (i.e. a PR across repos),
/// the fork is synced with upstream before submitting (GoReleaser parity).
///
/// Tries `gh` CLI first; if unavailable, falls back to the GitHub REST API
/// using the token from the RepositoryConfig (or `GITHUB_TOKEN` env var).
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_submit_pr(
    repo_path: &Path,
    repo: Option<&RepositoryConfig>,
    repo_owner: &str,
    repo_name: &str,
    branch_name: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) {
    let pr_cfg = match repo.and_then(|r| r.pull_request.as_ref()) {
        Some(pr) if pr.enabled == Some(true) => pr,
        _ => return,
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

    // PR creation: try gh CLI first, fall back to GitHub API.
    if gh_is_available() {
        create_pr_via_gh_cli(
            repo_path,
            &upstream_slug,
            &head,
            base_branch,
            title,
            pr_body,
            is_draft,
            label,
            log,
        );
    } else if let Some(ref tok) = token {
        create_pr_via_api(
            upstream_owner,
            upstream_name,
            &head,
            base_branch,
            title,
            pr_body,
            is_draft,
            tok,
            label,
            log,
        );
    } else {
        log.warn(&format!(
            "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
        ));
    }
}

/// Submit a pull request via the GitHub CLI. Logs a warning instead of failing
/// if `gh` is not available or the command exits non-zero.
///
/// Falls back to the GitHub REST API when `gh` is unavailable and a token
/// can be resolved from the environment.
pub(crate) fn submit_pr_via_gh(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) {
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

    if gh_is_available() {
        create_pr_via_gh_cli(
            repo_path,
            upstream_repo,
            head,
            &base_branch,
            title,
            body,
            false,
            label,
            log,
        );
    } else if let Some(ref tok) = token {
        if let Some((owner, name)) = upstream_repo.split_once('/') {
            create_pr_via_api(
                owner,
                name,
                head,
                &base_branch,
                title,
                body,
                false,
                tok,
                label,
                log,
            );
        } else {
            log.warn(&format!(
                "{label}: cannot parse upstream repo slug '{upstream_repo}' for API fallback"
            ));
        }
    } else {
        log.warn(&format!(
            "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
        ));
    }
}
