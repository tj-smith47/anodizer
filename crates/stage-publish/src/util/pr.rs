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

use super::branch::{fetch_default_branch, github_api_base};
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
    let base = github_api_base();
    let url = format!("{base}/repos/{owner}/{name}/pulls");
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
    match classify_pr_transport(gh_is_available(), token.is_some()) {
        PrTransport::GhCli => create_pr_via_gh_cli(repo_path, &upstream_slug, &spec, label, log),
        PrTransport::Api => {
            let tok = token
                .as_deref()
                .expect("classified Api implies token present");
            create_pr_via_api(&upstream, &spec, tok, label, log)
        }
        PrTransport::NoneAvailable => {
            let msg = format!(
                "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
            );
            log.warn(&msg);
            // Silent-fail (returning None here) would let dispatch
            // record Succeeded for a publisher that did no work.
            Some(PublisherOutcome::Failed(msg))
        }
    }
}

/// Which transport `maybe_submit_pr` / `submit_pr_via_gh_with_opts`
/// should dispatch to, given the runtime availability of `gh` and a
/// GitHub token. Pure data so unit tests can pin the decision without
/// touching the process env or PATH.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrTransport {
    /// `gh` CLI is on PATH; prefer it because it can force-push to
    /// update an existing PR's branch (the API transport cannot).
    GhCli,
    /// `gh` is missing but a token resolved — fall back to the
    /// GitHub REST API.
    Api,
    /// Neither `gh` nor a token is available. Callers must surface
    /// `PublisherOutcome::Failed` here; returning `None` silently
    /// would let dispatch record `Succeeded` for a publisher that
    /// did no work.
    NoneAvailable,
}

/// Pure classifier for the PR-transport decision. Extracted so the
/// `gh`/token preference is unit-testable without env-mutation.
pub(crate) fn classify_pr_transport(gh_available: bool, token_present: bool) -> PrTransport {
    if gh_available {
        PrTransport::GhCli
    } else if token_present {
        PrTransport::Api
    } else {
        PrTransport::NoneAvailable
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

    match classify_pr_transport(gh_is_available(), token.is_some()) {
        PrTransport::GhCli => create_pr_via_gh_cli(repo_path, upstream_repo, &spec, label, log),
        PrTransport::Api => {
            let tok = token
                .as_deref()
                .expect("classified Api implies token present");
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
        }
        PrTransport::NoneAvailable => {
            let msg = format!(
                "{label}: neither `gh` CLI nor a token is available -- cannot create PR automatically"
            );
            log.warn(&msg);
            // Silent-fail (returning None here) would let dispatch
            // record Succeeded for a publisher that did no work.
            Some(PublisherOutcome::Failed(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PrOrigin, PrSpec, PrTransport, Upstream, classify_pr_transport, create_pr_via_api,
        maybe_submit_pr,
    };
    use anodizer_core::PublisherOutcome;
    use anodizer_core::config::{
        HomebrewCaskConfig, KrewConfig, PullRequestConfig, RepositoryConfig, StringOrBool,
        WingetConfig,
    };
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::path::Path;

    fn quiet_log() -> StageLogger {
        StageLogger::new("pr-test", Verbosity::Quiet)
    }

    fn origin() -> PrOrigin<'static> {
        PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v1.2.3",
            update_existing_pr: false,
        }
    }

    /// gh on PATH wins regardless of whether a token is also present —
    /// gh can force-push to update an existing PR's branch, the API
    /// transport cannot.
    #[test]
    fn classify_pr_transport_prefers_gh_when_available() {
        assert_eq!(classify_pr_transport(true, false), PrTransport::GhCli);
        assert_eq!(classify_pr_transport(true, true), PrTransport::GhCli);
    }

    /// gh missing + token present → API fallback. This is the
    /// production path on CI runners that have GITHUB_TOKEN injected
    /// but no gh binary on PATH.
    #[test]
    fn classify_pr_transport_falls_back_to_api_when_gh_absent_and_token_present() {
        assert_eq!(classify_pr_transport(false, true), PrTransport::Api);
    }

    /// Neither available → NoneAvailable. The caller MUST map this to
    /// PublisherOutcome::Failed; returning None would let dispatch
    /// record Succeeded for a publisher that did no work. Pins the
    /// silent-skip contract fix at the decision-logic boundary.
    #[test]
    fn classify_pr_transport_neither_available_returns_none_available() {
        assert_eq!(
            classify_pr_transport(false, false),
            PrTransport::NoneAvailable,
            "callers map NoneAvailable -> PublisherOutcome::Failed; \
             returning None here would silently report Succeeded"
        );
    }

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

    /// `maybe_submit_pr` with `repo = None` must short-circuit to `None`
    /// before ever touching git, gh, or the network. Pins the
    /// "PR-disabled is the default" contract: a publisher that forgets
    /// to wire `repo` cannot accidentally open a PR.
    #[test]
    fn maybe_submit_pr_returns_none_when_repo_is_none() {
        let log = quiet_log();
        let outcome = maybe_submit_pr(
            Path::new("/nonexistent/should-never-be-touched"),
            None,
            &origin(),
            "title",
            "body",
            "label",
            &log,
        );
        assert!(
            outcome.is_none(),
            "repo=None must short-circuit before any git/gh/HTTP work"
        );
    }

    /// `maybe_submit_pr` with a repo whose `pull_request` field is
    /// `None` must short-circuit to `None`. Pins the
    /// pull_request-block-missing branch of the early-return guard.
    #[test]
    fn maybe_submit_pr_returns_none_when_pull_request_block_absent() {
        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            pull_request: None,
            ..Default::default()
        };
        let outcome = maybe_submit_pr(
            Path::new("/nonexistent/should-never-be-touched"),
            Some(&repo),
            &origin(),
            "title",
            "body",
            "label",
            &log,
        );
        assert!(
            outcome.is_none(),
            "pull_request=None must short-circuit before any git/gh/HTTP work"
        );
    }

    /// `maybe_submit_pr` with `pull_request.enabled = Some(false)` must
    /// short-circuit to `None`. Pins the explicit opt-out branch:
    /// configuring the block but leaving it disabled is NOT a green-light.
    #[test]
    fn maybe_submit_pr_returns_none_when_pull_request_disabled() {
        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let outcome = maybe_submit_pr(
            Path::new("/nonexistent/should-never-be-touched"),
            Some(&repo),
            &origin(),
            "title",
            "body",
            "label",
            &log,
        );
        assert!(
            outcome.is_none(),
            "pull_request.enabled=false must short-circuit before any git/gh/HTTP work"
        );
    }

    /// `maybe_submit_pr` with `pull_request.enabled = None` (the field
    /// defaulted) must short-circuit to `None`. Pins the
    /// "enabled defaults to off" contract — the early-return guard
    /// requires `enabled == Some(true)`, not just "block present".
    #[test]
    fn maybe_submit_pr_returns_none_when_pull_request_enabled_unset() {
        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            pull_request: Some(PullRequestConfig {
                enabled: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let outcome = maybe_submit_pr(
            Path::new("/nonexistent/should-never-be-touched"),
            Some(&repo),
            &origin(),
            "title",
            "body",
            "label",
            &log,
        );
        assert!(
            outcome.is_none(),
            "pull_request.enabled=None must short-circuit before any git/gh/HTTP work"
        );
    }

    // -----------------------------------------------------------------
    // `create_pr_via_api` HTTP coverage. Each test redirects requests
    // to an in-process responder by setting `ANODIZER_GITHUB_API_BASE`
    // under the workspace env mutex.
    // -----------------------------------------------------------------

    /// RAII guard that sets `ANODIZER_GITHUB_API_BASE` for the duration
    /// of one test and restores the previous value (or unsets) on drop
    /// so a panicking test body cannot leak the override.
    struct BaseOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl BaseOverride {
        fn set(base: &str) -> Self {
            let guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("ANODIZER_GITHUB_API_BASE").ok();
            // SAFETY: serialised by the workspace env mutex; pair set / restore.
            unsafe { std::env::set_var("ANODIZER_GITHUB_API_BASE", base) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for BaseOverride {
        fn drop(&mut self) {
            // SAFETY: still under the env mutex (held by `_guard`).
            unsafe {
                match &self.previous {
                    Some(prev) => std::env::set_var("ANODIZER_GITHUB_API_BASE", prev),
                    None => std::env::remove_var("ANODIZER_GITHUB_API_BASE"),
                }
            }
        }
    }

    fn spec(update_existing_pr: bool) -> PrSpec<'static> {
        PrSpec {
            title: "t",
            body: "b",
            head: "fork-owner:release/v1.2.3",
            base_branch: "main",
            draft: false,
            update_existing_pr,
        }
    }

    /// 201 Created is the success path — `create_pr_via_api` returns
    /// `None` so `Context::record_publisher_outcome` is NOT called and
    /// dispatch records `succeeded` from the default. A regression that
    /// returned `Some(Failed)` on 201 would gate every healthy PR.
    #[test]
    fn create_pr_via_api_returns_none_on_201() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome = create_pr_via_api(&upstream, &spec(false), "tok", "label", &log);
        assert!(outcome.is_none(), "201 must be the success path");
    }

    /// 422 with "already exists" + `update_existing_pr=false` is the
    /// "PR for this head already exists, leave it alone" branch.
    /// Returning `PendingValidation` pins the silent-skip fix at the
    /// transport boundary — dispatch would otherwise record this
    /// publisher as `succeeded` even though no PR was created.
    #[test]
    fn create_pr_via_api_returns_pending_on_422_already_exists_no_update() {
        let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:release/v1.2.3.\"}]}";
        let len = body.len();
        let resp =
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {len}\r\n\r\n{body}");
        let resp_static: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome = create_pr_via_api(&upstream, &spec(false), "tok", "label", &log);
        assert!(
            matches!(outcome, Some(PublisherOutcome::PendingValidation)),
            "422 already-exists must map to PendingValidation; got {outcome:?}"
        );
    }

    /// Same 422 body, but `update_existing_pr=true`. The API transport
    /// cannot force-push (no working tree), so the option is a
    /// documented no-op here and the outcome MUST still be
    /// `PendingValidation`. A regression that returned `None` on this
    /// branch would silently advertise success to dispatch even though
    /// the open PR did not advance to the new manifest.
    #[test]
    fn create_pr_via_api_returns_pending_on_422_already_exists_with_update() {
        let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:release/v1.2.3.\"}]}";
        let len = body.len();
        let resp =
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {len}\r\n\r\n{body}");
        let resp_static: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome = create_pr_via_api(&upstream, &spec(true), "tok", "label", &log);
        assert!(
            matches!(outcome, Some(PublisherOutcome::PendingValidation)),
            "API transport cannot force-push; update_existing_pr=true \
             must still surface PendingValidation, not None. Got {outcome:?}"
        );
    }

    /// 500 is a generic transport-style failure. The function must
    /// return `Some(Failed(_))` so dispatch records the truth — a
    /// regression that returned `None` would let dispatch record
    /// `succeeded` for a publisher that did no work (the silent-skip
    /// bug class fixed by the `must_use` return values throughout
    /// stage-publish).
    #[test]
    fn create_pr_via_api_returns_failed_on_500() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome = create_pr_via_api(&upstream, &spec(false), "tok", "label", &log);
        assert!(
            matches!(outcome, Some(PublisherOutcome::Failed(_))),
            "500 must map to Failed (silent-skip would let dispatch \
             record succeeded); got {outcome:?}"
        );
    }
}
