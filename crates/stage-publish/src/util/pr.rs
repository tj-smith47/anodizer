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
use anodizer_core::run::run_capture_timeout;
use anodizer_core::{EnvSource, ProcessEnvSource};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::branch::{fetch_default_branch_with_env, github_api_base_from};
use super::cmd::{run_cmd_in, run_cmd_in_timeout};

/// Wall-clock bound on `gh pr create` — a lightweight PR submission against the
/// GitHub API. A hung API call would otherwise hang the release with no
/// deadline; on expiry the subtree is killed and the attempt retries within the
/// existing 3-try loop. Sized for a remote metadata/PR-submission call.
const GH_PR_CREATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wall-clock bound on the `git fetch upstream` fork-sync. The fetch hits the
/// upstream remote; sync is best-effort (a failure, incl. a deadline kill, only
/// warns and proceeds), so a stalled fetch must not hang the release. Sized as a
/// remote fetch.
const GIT_FETCH_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Wall-clock bound on the force-push that updates an existing PR's branch. A
/// wedged push must not hang the release; on expiry the subtree is killed and
/// the failure warns (the PR simply isn't updated in place). Sized as a remote
/// push.
const GIT_FORCE_PUSH_TIMEOUT: Duration = Duration::from_secs(600);

/// Sync a fork with its upstream base repository.
///
/// When PR mode targets a different (upstream) repository, the fork may be
/// behind.  This fetches the upstream base branch and rebases local work on
/// top, syncing the fork with upstream.
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

    // Fetch the upstream base branch. Bounded: it hits the upstream remote, so
    // a stalled fetch must not hang the release; a deadline kill warns like any
    // other sync failure and proceeds.
    if let Err(e) = run_cmd_in_timeout(
        repo_path,
        "git",
        &["fetch", "upstream", base_branch],
        &format!("{label}: git fetch upstream"),
        None,
        log,
        GIT_FETCH_UPSTREAM_TIMEOUT,
    ) {
        log.warn(&format!(
            "failed to fetch upstream for {label} fork sync; continuing without sync: {e}"
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
            "failed to rebase {label} fork onto upstream; aborting rebase and continuing: {e}"
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

/// Check whether the `gh` CLI is available (spawn probe). A probe FAILURE
/// (permission denied, exec-format — presence unknown) is surfaced as a
/// WARN before falling back to the token/API transport, never silently
/// collapsed into "gh absent": a quiet reroute would hide a broken gh from
/// the operator while changing which transport (and which capabilities,
/// e.g. force-push PR updates) the publish uses.
fn gh_is_available(log: &StageLogger) -> bool {
    match anodizer_core::tool_detect::runs("gh") {
        anodizer_core::tool_detect::ToolProbe::Available => true,
        anodizer_core::tool_detect::ToolProbe::Unavailable => false,
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            log.warn(&format!(
                "could not probe gh availability ({e}); falling back to the \
                 token/API transport"
            ));
            false
        }
    }
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
        "skipped {label} PR for '{head}' — already exists \
         (set update_existing_pr: true to update the PR in place)"
    )
}

/// Status message rendered when the gh CLI reports the PR already
/// exists, `update_existing_pr` is true, and the force-push to the
/// existing branch succeeded.
pub(crate) fn pr_exists_update_status_message(label: &str, head: &str) -> String {
    format!("{label} PR for '{head}' already exists — updated in place")
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
        // Bounded: `gh pr create` hits the GitHub API, so a hung call must not
        // hang the release. A deadline kill is Retriable → consumed by this
        // loop's retry path; a spawn failure stays the hard `Failed` below.
        let mut gh_cmd = Command::new("gh");
        gh_cmd.current_dir(repo_path).args(&args);
        let pr_result = run_capture_timeout(
            &mut gh_cmd,
            log,
            &format!("{label}: gh pr create"),
            GH_PR_CREATE_TIMEOUT,
        );
        match pr_result {
            Ok(output) if output.status.success() => {
                log.status(&format!("submitted {label} PR via gh CLI"));
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
                        if let Err(e) = run_cmd_in_timeout(
                            repo_path,
                            "git",
                            &["push", "--force-with-lease", "origin", branch_name],
                            &format!("{label}: git push --force-with-lease (update existing PR)"),
                            None,
                            log,
                            GIT_FORCE_PUSH_TIMEOUT,
                        ) {
                            log.warn(&format!(
                                "failed to force-push {label} PR branch (update_existing_pr=true): {e}"
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
                    "gh pr create for {label} hit transient error (attempt {attempt}/3); retrying..."
                ));
                std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
            }
            Err(e) => {
                // A deadline kill (the API call stalled) is Retriable and
                // transient — consume it on the same 3-try path as a transient
                // "No commits between …", rather than hard-failing on a hang.
                if anodizer_core::retry::is_retriable(e.as_ref()) && attempt < 3 {
                    last_stderr = format!("{e:#}");
                    log.warn(&format!(
                        "gh pr create for {label} timed out (attempt {attempt}/3); retrying..."
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
                    continue;
                }
                let msg = format!(
                    "could not run gh to create the {label} PR: {e} -- you may need to create the PR manually"
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
        "gh pr create for {label} exited with {} -- you may need to create the PR manually{}",
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
///
/// Production callers thread `ctx.env_source()` so the
/// `ANODIZER_GITHUB_API_BASE` override (undocumented test hook) routes
/// through the injected [`EnvSource`]. Tests pass a
/// [`MapEnvSource`](anodizer_core::MapEnvSource) so the in-process
/// responder address is read from the map instead of the process env.
fn create_pr_via_api_with_env<E: EnvSource + ?Sized>(
    upstream: &Upstream<'_>,
    spec: &PrSpec<'_>,
    token: &str,
    label: &str,
    log: &StageLogger,
    env: &E,
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
    let base = github_api_base_from(env);
    let url = format!("{base}/repos/{owner}/{name}/pulls");
    let payload = serde_json::json!({
        "title": title, "head": head, "base": base_branch, "body": body, "draft": draft,
    });
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to build HTTP client for {label}: {e}");
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
            log.status(&format!("submitted {label} PR via GitHub API"));
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
                        "{label} PR for '{head}' already exists and update_existing_pr=true \
                         was requested, but the API transport cannot force-push; \
                         install `gh` CLI to update the PR in place"
                    ));
                } else {
                    log.warn(&pr_exists_skip_warn_message(label, head));
                }
                return Some(PublisherOutcome::PendingValidation);
            }
            let msg = format!(
                "GitHub API PR creation for {label} returned {status} -- you may need to create the PR manually\n{body_text}"
            );
            log.warn(&msg);
            Some(PublisherOutcome::Failed(msg))
        }
        Err(e) => {
            let msg = format!(
                "GitHub API PR creation for {label} failed: {e} -- you may need to create the PR manually"
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
/// the fork is synced with upstream before submitting.
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_submit_pr(
    repo_path: &Path,
    repo: Option<&RepositoryConfig>,
    origin: &PrOrigin<'_>,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
    render: &dyn Fn(&str) -> String,
) -> Option<PublisherOutcome> {
    maybe_submit_pr_with_env(
        repo_path,
        repo,
        origin,
        title,
        body,
        label,
        log,
        render,
        &ProcessEnvSource,
    )
}

/// Env-injectable sibling of [`maybe_submit_pr`]. Production routes
/// through the no-env form (delegating to [`ProcessEnvSource`]); the
/// caller wires up `ctx.env_source()` when threading the lookup through
/// a [`crate::Context`].
#[allow(clippy::too_many_arguments)]
#[must_use = "the returned outcome override must be forwarded to \
              Context::record_publisher_outcome — dropping it silently \
              misreports a PR-already-exists skip or a PR-creation failure \
              as `succeeded`"]
pub(crate) fn maybe_submit_pr_with_env<E: EnvSource + ?Sized>(
    repo_path: &Path,
    repo: Option<&RepositoryConfig>,
    origin: &PrOrigin<'_>,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
    render: &dyn Fn(&str) -> String,
    env: &E,
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
    // `base.owner` / `base.name` / `base.branch` / `body` may be templated;
    // render them before they reach the upstream slug / API payload, mirroring
    // how the owner/name of the fork are already rendered upstream.
    let (upstream_owner, upstream_name) = if let Some(ref base) = pr_cfg.base {
        (
            base.owner
                .as_deref()
                .map(render)
                .unwrap_or_else(|| repo_owner.to_string()),
            base.name
                .as_deref()
                .map(render)
                .unwrap_or_else(|| repo_name.to_string()),
        )
    } else {
        (repo_owner.to_string(), repo_name.to_string())
    };
    let upstream_owner = upstream_owner.as_str();
    let upstream_name = upstream_name.as_str();
    let upstream_slug = format!("{}/{}", upstream_owner, upstream_name);
    let pr_body = pr_cfg
        .body
        .as_deref()
        .map(render)
        .unwrap_or_else(|| body.to_string());
    let head = format!("{}:{}", repo_owner, branch_name);
    let is_draft = pr_cfg.draft == Some(true);
    let base_branch = pr_cfg
        .base
        .as_ref()
        .and_then(|b| b.branch.as_deref())
        .map(render)
        .unwrap_or_else(|| "main".to_string());
    // A configured `repository.token` may be templated; render it before it
    // becomes the API bearer credential, or the literal template is sent.
    // The canonical resolver applies the empty-string filter at every link
    // (including the rendered `repository.token`), so a blank value falls
    // through to ANODIZER_GITHUB_TOKEN -> GITHUB_TOKEN.
    let explicit = repo.and_then(|r| r.token.as_deref()).map(render);
    let token =
        anodizer_core::git::resolve_github_token_with_env(explicit.as_deref(), &|key| env.var(key));

    // Fork sync: when the PR targets a different upstream repository, sync first.
    let is_cross_repo = upstream_owner != repo_owner || upstream_name != repo_name;
    if is_cross_repo {
        let upstream_url = format!(
            "https://github.com/{}/{}.git",
            upstream_owner, upstream_name
        );
        sync_fork(repo_path, &upstream_url, &base_branch, label, log);
        if let Err(e) = run_cmd_in(
            repo_path,
            "git",
            &["push", "--force-with-lease", "origin", branch_name],
            &format!("{label}: git push (post-sync)"),
        ) {
            log.warn(&format!(
                "failed to force-push {label} fork after rebase; PR may have conflicts: {e}"
            ));
        }
    }

    let spec = PrSpec {
        title,
        body: &pr_body,
        head: &head,
        base_branch: &base_branch,
        draft: is_draft,
        update_existing_pr,
    };
    let upstream = Upstream {
        owner: upstream_owner,
        name: upstream_name,
    };

    // PR creation: try gh CLI first, fall back to GitHub API.
    match classify_pr_transport(gh_is_available(log), token.is_some()) {
        PrTransport::GhCli => create_pr_via_gh_cli(repo_path, &upstream_slug, &spec, label, log),
        PrTransport::Api => {
            let tok = token
                .as_deref()
                .expect("classified Api implies token present");
            create_pr_via_api_with_env(&upstream, &spec, tok, label, log, env)
        }
        PrTransport::NoneAvailable => {
            let msg = format!(
                "cannot create the {label} PR automatically -- neither `gh` CLI nor a token is available"
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
    submit_pr_via_gh_with_opts_with_env(
        repo_path,
        upstream_repo,
        head,
        title,
        body,
        label,
        log,
        opts,
        &ProcessEnvSource,
    )
}

/// Env-injectable sibling of [`submit_pr_via_gh_with_opts`]. Production
/// routes through the no-env form (delegating to [`ProcessEnvSource`]);
/// the caller wires `ctx.env_source()` when threading lookups through a
/// [`crate::Context`].
#[allow(clippy::too_many_arguments)]
#[must_use = "the returned outcome override must be forwarded to \
              Context::record_publisher_outcome — dropping it silently \
              misreports a PR-already-exists skip or a PR-creation failure \
              as `succeeded`"]
pub(crate) fn submit_pr_via_gh_with_opts_with_env<E: EnvSource + ?Sized>(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
    opts: SubmitPrOpts,
    env: &E,
) -> Option<PublisherOutcome> {
    let token = anodizer_core::git::resolve_github_token_with_env(None, &|key| env.var(key));

    // Discover the upstream's actual default branch. Hardcoding "main" breaks
    // PR creation against repos whose default is "master" (e.g.
    // microsoft/winget-pkgs) or any other name. The 404 on the base ref
    // bubbles up as a tangled GraphQL error from `gh pr create`:
    // "Head sha can't be blank, ..., not all refs are readable, Base ref
    // must be a branch". Fall back to "main" only if the lookup fails.
    let base_branch = upstream_repo
        .split_once('/')
        .and_then(|(owner, name)| fetch_default_branch_with_env(owner, name, token.as_deref(), env))
        .unwrap_or_else(|| "main".to_string());

    let spec = PrSpec {
        title,
        body,
        head,
        base_branch: &base_branch,
        draft: false,
        update_existing_pr: opts.update_existing_pr,
    };

    match classify_pr_transport(gh_is_available(log), token.is_some()) {
        PrTransport::GhCli => create_pr_via_gh_cli(repo_path, upstream_repo, &spec, label, log),
        PrTransport::Api => {
            let tok = token
                .as_deref()
                .expect("classified Api implies token present");
            if let Some((owner, name)) = upstream_repo.split_once('/') {
                let upstream = Upstream { owner, name };
                create_pr_via_api_with_env(&upstream, &spec, tok, label, log, env)
            } else {
                let msg = format!(
                    "cannot parse upstream repo slug '{upstream_repo}' for the {label} API fallback"
                );
                log.warn(&msg);
                Some(PublisherOutcome::Failed(msg))
            }
        }
        PrTransport::NoneAvailable => {
            let msg = format!(
                "cannot create the {label} PR automatically -- neither `gh` CLI nor a token is available"
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
    #[cfg(unix)]
    use super::maybe_submit_pr_with_env;
    use super::{
        PrOrigin, PrSpec, PrTransport, Upstream, classify_pr_transport, create_pr_via_api_with_env,
        gh_is_available, maybe_submit_pr, sync_fork,
    };
    use anodizer_core::MapEnvSource;
    use anodizer_core::PublisherOutcome;
    #[cfg(unix)]
    use anodizer_core::config::PullRequestBaseConfig;
    use anodizer_core::config::{
        HomebrewCaskConfig, KrewConfig, PullRequestConfig, RepositoryConfig, StringOrBool,
        WingetConfig,
    };
    use anodizer_core::log::{StageLogger, Verbosity};
    // Consumed only by the unix-gated `gh_absent_path` helper below; the gate
    // must match or the import reads as unused on a Windows build.
    #[cfg(unix)]
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    // `#[serial(path_env)]` appears only on the unix-gated PATH-stub tests
    // below; the gate must match or the import reads as unused on a Windows
    // build.
    #[cfg(unix)]
    use serial_test::serial;
    use std::path::Path;
    use std::process::Command;
    use std::sync::OnceLock;

    fn quiet_log() -> StageLogger {
        StageLogger::new("pr-test", Verbosity::Quiet)
    }

    /// Identity renderer for the PR helpers' template pass — these tests
    /// feed plain (non-templated) PR fields, so rendering is a no-op.
    fn no_render(s: &str) -> String {
        s.to_string()
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
    /// The probe delegates to `core::tool_detect::runs("gh")`; the outcome
    /// depends on whether gh is on PATH, so only assert it returns a bool
    /// without panicking.
    #[test]
    fn gh_is_available_returns_a_bool_without_panicking() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let _: bool = gh_is_available(&log);
    }

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
            &no_render,
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
            &no_render,
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
            &no_render,
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
            &no_render,
        );
        assert!(
            outcome.is_none(),
            "pull_request.enabled=None must short-circuit before any git/gh/HTTP work"
        );
    }

    // -----------------------------------------------------------------
    // `create_pr_via_api_with_env` HTTP coverage. Each test redirects
    // requests to an in-process responder by injecting
    // `ANODIZER_GITHUB_API_BASE` through a [`MapEnvSource`] — no process
    // env mutation, no env mutex acquisition, no shared state.
    // -----------------------------------------------------------------

    fn env_with_base(base: &str) -> MapEnvSource {
        MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", base)
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
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
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
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
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
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(true), "tok", "label", &log, &env);
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
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        assert!(
            matches!(outcome, Some(PublisherOutcome::Failed(_))),
            "500 must map to Failed (silent-skip would let dispatch \
             record succeeded); got {outcome:?}"
        );
    }

    /// 4xx (e.g. 401 Unauthorized) that is NOT the 422 already-exists
    /// pattern maps to Failed — the API rejected the request and the
    /// PR was not created. Pins the bucket-failures-as-Failed contract
    /// for the catch-all branch in `create_pr_via_api`.
    #[test]
    fn create_pr_via_api_returns_failed_on_401() {
        let body = "{\"message\":\"Bad credentials\"}";
        let len = body.len();
        let resp = format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {len}\r\n\r\n{body}");
        let resp_static: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        match outcome {
            Some(PublisherOutcome::Failed(msg)) => {
                assert!(msg.contains("401"), "Failed msg must cite status: {msg}");
                assert!(msg.contains("Bad credentials"), "must include body: {msg}");
            }
            other => panic!("expected Failed on 401, got {other:?}"),
        }
    }

    /// 403 Forbidden also maps to Failed (rate-limited tokens hit this
    /// frequently). Confirms the catch-all branch is not 5xx-only.
    #[test]
    fn create_pr_via_api_returns_failed_on_403() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        ]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        assert!(matches!(outcome, Some(PublisherOutcome::Failed(_))));
    }

    /// 200 OK is treated as success by `is_success()` (GitHub returns
    /// 201 in practice, but the function's contract is "any 2xx").
    #[test]
    fn create_pr_via_api_returns_none_on_200_ok() {
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        assert!(outcome.is_none(), "any 2xx is success path");
    }

    /// 422 with a body that does NOT mention "already exists" must NOT
    /// be reclassified as PendingValidation — it stays a Failed. Pins
    /// the body-text discriminator that separates the legitimate
    /// "validation error" case (e.g. invalid base ref) from the
    /// "PR already exists" duplicate case.
    #[test]
    fn create_pr_via_api_422_without_already_exists_phrase_is_failed() {
        let body =
            "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"Invalid base ref\"}]}";
        let len = body.len();
        let resp =
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {len}\r\n\r\n{body}");
        let resp_static: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        assert!(
            matches!(outcome, Some(PublisherOutcome::Failed(_))),
            "422 without 'already exists' phrase is Failed; got {outcome:?}"
        );
    }

    /// Transport-layer failure: no responder is listening on the address
    /// the override points at. The function must return Failed (not panic
    /// and not silently return None) so dispatch records the real outcome.
    #[test]
    fn create_pr_via_api_transport_failure_returns_failed() {
        // Pick a fixed address that's almost certainly unbound. Any
        // unreachable destination triggers the `Err(e)` arm.
        let env = env_with_base("http://127.0.0.1:1");
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &spec(false), "tok", "label", &log, &env);
        assert!(
            matches!(outcome, Some(PublisherOutcome::Failed(_))),
            "transport failure must map to Failed; got {outcome:?}"
        );
    }

    /// `pr_exists_skip_warn_message` is empty-label-safe — the label is
    /// the publisher name (homebrew, winget, ...) and never an attacker-
    /// controlled string, but a future refactor must not surprise-skip
    /// formatting when the label is "".
    #[test]
    fn pr_exists_skip_warn_message_handles_empty_label() {
        let msg = super::pr_exists_skip_warn_message("", "owner:branch");
        assert!(msg.contains("already exists"));
        assert!(msg.contains("owner:branch"));
    }

    /// `maybe_submit_pr` short-circuits when origin's `update_existing_pr`
    /// is set but `pull_request` config is None — the flag has no effect
    /// without the gating block. Defends "knob without container does
    /// nothing" expectation.
    #[test]
    fn maybe_submit_pr_update_existing_pr_alone_does_not_open_pr() {
        let log = quiet_log();
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v1.2.3",
            update_existing_pr: true,
        };
        let outcome = maybe_submit_pr(
            Path::new("/nonexistent/should-never-be-touched"),
            None,
            &origin,
            "title",
            "body",
            "label",
            &log,
            &no_render,
        );
        assert!(outcome.is_none());
    }

    // =================================================================
    // Local-git-fixture infrastructure for the flows that shell out to
    // `git` (`sync_fork`, and `maybe_submit_pr_with_env`'s cross-repo
    // fork-sync + post-sync force-push). All of these run against a
    // local bare repo reached over a `file://`-equivalent filesystem
    // path — no network. Mirrors the `init_bare_remote_with_one_commit`
    // helper in `util/git_revert.rs`.
    // =================================================================

    /// Give the test process a git identity so the helper's `git commit`
    /// works on bare CI runners (no global ~/.gitconfig). Set once per
    /// process via `OnceLock` to avoid the parallel-test `set_var` race.
    /// Mirrors `util/git_revert.rs::ensure_git_identity`.
    fn ensure_git_identity() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            // SAFETY: runs exactly once per process, guarded by OnceLock;
            // values are constants, not user input.
            unsafe {
                std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                // Fail the cross-repo fork-sync `git fetch <https-upstream>`
                // immediately instead of blocking on an interactive
                // credential prompt — these tests never reach a real host.
                std::env::set_var("GIT_TERMINAL_PROMPT", "0"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            }
        });
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Configure a working dir as a usable git repo (identity +
    /// no-gpg-sign) on branch `master`.
    fn git_init_work(dir: &Path) {
        git_ok(dir, &["init", "-b", "master"]);
        git_ok(dir, &["config", "user.email", "test@example.invalid"]);
        git_ok(dir, &["config", "user.name", "Test"]);
        git_ok(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn write_commit(dir: &Path, file: &str, contents: &str, msg: &str) {
        std::fs::write(dir.join(file), contents).unwrap();
        git_ok(dir, &["add", file]);
        git_ok(dir, &["commit", "-m", msg]);
    }

    /// Build a bare "origin" remote with one commit on `master`, plus a
    /// working clone already wired to push to it. Returns
    /// `(origin_path_string, _bare_holder, work_holder)`.
    fn init_origin_with_work() -> (String, tempfile::TempDir, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare tempdir");
        let work = tempfile::tempdir().expect("work tempdir");

        git_ok(bare.path(), &["init", "--bare", "-b", "master"]);

        git_init_work(work.path());
        write_commit(work.path(), "README", "hello\n", "initial commit");
        // `git remote add` takes a path; pass it as an OsStr arg.
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git remote add origin failed"
        );
        git_ok(work.path(), &["push", "-u", "origin", "master"]);

        let origin_url = bare.path().to_string_lossy().into_owned();
        (origin_url, bare, work)
    }

    /// Build a bare "upstream" remote that is one commit AHEAD of the
    /// shared base commit, so a rebase against it advances the rebasing
    /// repo. The upstream and the work clone share the same root commit
    /// (work is cloned from upstream) which keeps the rebase conflict-free.
    /// Returns `(upstream_path, work_holder, _upstream_holder)`.
    fn init_upstream_ahead_with_clone() -> (String, tempfile::TempDir, tempfile::TempDir) {
        ensure_git_identity();
        let upstream = tempfile::tempdir().expect("upstream tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        let work = tempfile::tempdir().expect("work tempdir");

        // Seed the upstream bare repo with one commit on master.
        git_ok(upstream.path(), &["init", "--bare", "-b", "master"]);
        git_init_work(seed.path());
        write_commit(seed.path(), "base.txt", "base\n", "base commit");
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(upstream.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        git_ok(seed.path(), &["push", "-u", "origin", "master"]);

        // Clone the upstream into `work` (this shares history root).
        let upstream_url = upstream.path().to_string_lossy().into_owned();
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["clone"]).arg(upstream.path()).arg(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git clone for work failed"
        );
        git_ok(
            work.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git_ok(work.path(), &["config", "user.name", "Test"]);
        git_ok(work.path(), &["config", "commit.gpgsign", "false"]);

        // Advance upstream by one commit AFTER the work clone was taken,
        // so `sync_fork`'s rebase has something to fast-forward over.
        write_commit(seed.path(), "ahead.txt", "ahead\n", "upstream advance");
        git_ok(seed.path(), &["push", "origin", "master"]);

        (upstream_url, work, upstream)
    }

    // -----------------------------------------------------------------
    // `sync_fork`
    // -----------------------------------------------------------------

    /// Success path: when the upstream base branch carries a commit the
    /// fork doesn't have yet, `sync_fork` fetches upstream and rebases the
    /// fork's local branch on top — the fork's working tree gains the
    /// upstream-only file. Asserts the rebase actually advanced history,
    /// not merely that the function returned.
    #[test]
    fn sync_fork_rebases_fork_onto_upstream_advance() {
        let (upstream_url, work, _upstream) = init_upstream_ahead_with_clone();
        let log = quiet_log();

        // Before sync, the work clone has NOT seen the upstream advance.
        assert!(
            !work.path().join("ahead.txt").exists(),
            "precondition: work clone must not yet have the upstream-only file"
        );

        sync_fork(work.path(), &upstream_url, "master", "sync-test", &log);

        // After rebase onto upstream/master the upstream-only file is present
        // in the fork's working tree.
        assert!(
            work.path().join("ahead.txt").exists(),
            "sync_fork must rebase the fork onto upstream/master, pulling in \
             the upstream-only commit (ahead.txt)"
        );
        // And the upstream advance commit is now in the fork's history.
        let subjects = git_stdout(work.path(), &["log", "--pretty=%s"]);
        assert!(
            subjects.contains("upstream advance"),
            "rebased history must contain the upstream commit; got:\n{subjects}"
        );
    }

    /// Fetch-failure path: a bogus upstream URL makes `git fetch upstream`
    /// fail. `sync_fork` must swallow the error (best-effort), leave the
    /// fork's HEAD untouched, and not panic — the push still proceeds with
    /// the un-synced branch.
    #[test]
    fn sync_fork_fetch_failure_leaves_head_untouched() {
        let (_origin, _bare, work) = init_origin_with_work();
        let log = quiet_log();
        let head_before = git_stdout(work.path(), &["rev-parse", "HEAD"]);

        // Point upstream at a path that is not a git repo at all.
        let bogus = tempfile::tempdir().expect("bogus tempdir");
        let bogus_url = bogus.path().to_string_lossy().into_owned();
        sync_fork(work.path(), &bogus_url, "master", "sync-test", &log);

        let head_after = git_stdout(work.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            head_before, head_after,
            "a failed upstream fetch must leave the fork's HEAD unchanged"
        );
    }

    /// Idempotence / re-entry: calling `sync_fork` twice must not fail on
    /// the second `git remote add upstream` (the remote already exists).
    /// The function explicitly ignores that error; this pins the contract
    /// so a future refactor doesn't start bailing on the duplicate-remote
    /// case.
    #[test]
    fn sync_fork_tolerates_preexisting_upstream_remote() {
        let (upstream_url, work, _upstream) = init_upstream_ahead_with_clone();
        let log = quiet_log();

        sync_fork(work.path(), &upstream_url, "master", "sync-test", &log);
        // Second call: `git remote add upstream` now errors (already exists)
        // but sync_fork must continue and re-rebase cleanly (no-op rebase).
        sync_fork(work.path(), &upstream_url, "master", "sync-test", &log);

        assert!(
            work.path().join("ahead.txt").exists(),
            "second sync_fork call must still leave the fork synced; \
             the duplicate `git remote add upstream` error is ignored"
        );
    }

    // -----------------------------------------------------------------
    // `maybe_submit_pr_with_env` — cross-repo fork-sync + force-push side
    // effects. These run BEFORE the transport dispatch, so they are
    // observable on the local `file://` origin. To keep the transport
    // hermetic (the box may have a real `gh` that would call github.com),
    // each test installs a failing `gh` stub via `FakeToolDir` so
    // `gh_is_available()` returns false, forcing the token-driven API
    // transport onto an in-process scripted responder. Both touch
    // `PATH`, so they hold the env mutex and run `#[serial(path_env)]`
    // (the shared crate-wide PATH-mutation group).
    // -----------------------------------------------------------------

    /// Install a `gh` stub that exits non-zero on `--version` so
    /// `gh_is_available()` reports false, then prepend it to `PATH`.
    /// Returns the guard (restores `PATH` + releases the env mutex on
    /// drop) plus the `FakeToolDir` holder (keeps the stub on disk).
    #[cfg(unix)]
    fn gh_absent_path() -> (
        FakeToolDir,
        anodizer_core::test_helpers::fake_tool::PathGuard,
    ) {
        let tools = FakeToolDir::new();
        // `gh --version` exits 1 => probe treats gh as unavailable.
        tools.tool("gh").exit(1).install();
        let guard = tools.activate();
        (tools, guard)
    }

    /// Cross-repo PR (upstream owner/name differ from the fork) takes the
    /// fork-sync branch and then dispatches the PR via the API transport.
    /// Two observable effects are asserted:
    ///   1. the fork's `branch_name` was force-pushed to the local origin
    ///      (the post-sync `git push --force-with-lease origin <branch>`),
    ///   2. the PR-create request reached the responder at
    ///      `/repos/upstream-owner/upstream-repo/pulls` with the correct
    ///      head/base — proving the cross-repo upstream resolution flows
    ///      through to the request, not the fork's own slug.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn maybe_submit_pr_cross_repo_force_pushes_and_targets_upstream() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v9.9.9"]);
        write_commit(work.path(), "manifest.rb", "formula\n", "add manifest");

        // Responder accepts the cross-repo PR create. A separate GET
        // route would be the default-branch lookup, but `maybe_submit_pr`
        // takes the base branch from config (`master`), not a lookup, so
        // only the POST is expected here.
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/upstream-owner/upstream-repo/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            token: Some("ghp_test".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                // Different upstream owner/name => cross-repo => fork sync.
                base: Some(PullRequestBaseConfig {
                    owner: Some("upstream-owner".into()),
                    name: Some("upstream-repo".into()),
                    branch: Some("master".into()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v9.9.9",
            update_existing_pr: false,
        };

        let env = MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "body",
            "homebrew",
            &log,
            &no_render,
            &env,
        );
        // 201 from the responder is the success path.
        assert!(
            outcome.is_none(),
            "201 PR create is success; got {outcome:?}"
        );

        // (1) Force-push side effect: the release branch landed in origin.
        let refs = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            refs.contains("release/v9.9.9"),
            "cross-repo flow must force-push the release branch to origin; \
             bare branches:\n{refs}"
        );
        let subject = git_stdout(bare.path(), &["log", "-1", "--pretty=%s", "release/v9.9.9"]);
        assert_eq!(
            subject, "add manifest",
            "force-pushed branch must carry the manifest commit"
        );

        // (2) The PR request targeted the UPSTREAM repo, head = fork:branch.
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
        assert_eq!(entries[0].path, "/repos/upstream-owner/upstream-repo/pulls");
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(
            payload["head"], "fork-owner:release/v9.9.9",
            "head must be fork-owner:branch"
        );
        assert_eq!(
            payload["base"], "master",
            "base branch from pull_request.base"
        );
        drop(work);
    }

    /// Same-repo PR (upstream == fork) must NOT take the cross-repo
    /// fork-sync branch — the release branch is NOT force-pushed to origin
    /// as a side effect — yet the PR is still created, targeting the fork's
    /// own slug. Pins the `is_cross_repo` guard AND that the non-cross-repo
    /// path still reaches the transport with the fork as upstream.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn maybe_submit_pr_same_repo_skips_sync_but_still_creates_pr() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v8.8.8"]);
        write_commit(work.path(), "manifest.rb", "formula\n", "add manifest");

        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/fork-repo/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            token: Some("ghp_test".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                // No `base` => upstream defaults to the fork => same-repo.
                base: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v8.8.8",
            update_existing_pr: false,
        };
        let env = MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "body",
            "homebrew",
            &log,
            &no_render,
            &env,
        );
        assert!(
            outcome.is_none(),
            "201 PR create is success; got {outcome:?}"
        );

        // No cross-repo => no force-push => origin still only has master.
        let refs = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !refs.contains("release/v8.8.8"),
            "same-repo PR must NOT force-push the release branch; \
             bare branches should still be just master:\n{refs}"
        );

        // But the PR WAS created, targeting the fork's own slug.
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "same-repo PR must still POST a create");
        assert_eq!(entries[0].path, "/repos/fork-owner/fork-repo/pulls");
        drop(work);
    }

    /// `pull_request.body` overrides the caller-supplied body; `draft:
    /// true` flips the payload's draft flag. Drives the full
    /// `maybe_submit_pr_with_env` flow (same-repo, hermetic gh-absent
    /// transport) and asserts the emitted request reflects both config
    /// fields — the config-to-request wiring, not just the return value.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn maybe_submit_pr_applies_body_override_and_draft_flag() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, _bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v7.7.7"]);
        write_commit(work.path(), "manifest.rb", "formula\n", "add manifest");

        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/fork-repo/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            token: Some("ghp_test".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                draft: Some(true),
                body: Some("PR body from config".into()),
                base: None,
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v7.7.7",
            update_existing_pr: false,
        };
        let env = MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "caller-supplied body that must be overridden",
            "homebrew",
            &log,
            &no_render,
            &env,
        );
        assert!(outcome.is_none(), "201 is success; got {outcome:?}");

        let entries = req_log.lock().unwrap();
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(
            payload["body"], "PR body from config",
            "pull_request.body must override the caller body"
        );
        assert_eq!(
            payload["draft"], true,
            "pull_request.draft must set the flag"
        );
        drop(work);
    }

    /// Token resolution precedence: with NO `repo.token`, the flow falls
    /// back to `ANODIZER_GITHUB_TOKEN` from the env source. With gh absent
    /// and that var present, the API transport is selected and the PR is
    /// created — proving env-based token resolution reaches the transport.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn maybe_submit_pr_resolves_token_from_env_when_repo_token_absent() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, _bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v6.6.6"]);
        write_commit(work.path(), "manifest.rb", "formula\n", "add manifest");

        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/fork-repo/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            // No token on the repo config — must fall back to env.
            token: None,
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                base: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v6.6.6",
            update_existing_pr: false,
        };
        let env = MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
            .with("ANODIZER_GITHUB_TOKEN", "env-token");
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "body",
            "homebrew",
            &log,
            &no_render,
            &env,
        );
        assert!(
            outcome.is_none(),
            "env-resolved token must reach the API transport (201); got {outcome:?}"
        );
        let entries = req_log.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "env-resolved token must drive a PR-create POST"
        );
        drop(work);
    }

    /// No token anywhere (repo config absent, env unset) AND gh absent =>
    /// `NoneAvailable` => the function MUST surface `Failed`, never a
    /// silent `None` that dispatch would record as `succeeded`. End-to-end
    /// proof of the silent-skip contract at the orchestration boundary.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn maybe_submit_pr_no_gh_no_token_returns_failed() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, _bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v5.5.5"]);
        write_commit(work.path(), "manifest.rb", "formula\n", "add manifest");

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            token: None,
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                base: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v5.5.5",
            update_existing_pr: false,
        };
        // MapEnvSource carries NO token vars, so token resolution yields
        // None; gh stub is unavailable => NoneAvailable.
        let env = MapEnvSource::new();
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "body",
            "homebrew",
            &log,
            &no_render,
            &env,
        );
        match outcome {
            Some(PublisherOutcome::Failed(msg)) => {
                assert!(
                    msg.contains("neither") && msg.contains("gh"),
                    "Failed msg must explain neither gh nor token available: {msg}"
                );
            }
            other => panic!("expected Failed when neither gh nor token present, got {other:?}"),
        }
        drop(work);
    }

    // -----------------------------------------------------------------
    // `create_pr_via_api_with_env` — REQUEST-CONTENT assertions. The
    // existing tests assert the returned outcome per status code; these
    // pin the actual request the API transport emits (method, path, and
    // JSON payload fields) via the scripted responder's request log.
    // -----------------------------------------------------------------

    /// The API transport must POST to
    /// `/repos/{owner}/{name}/pulls` with the spec's title/head/base/body
    /// and `draft` flag serialised into the JSON payload. Asserts on the
    /// recorded request, not just the 201 outcome.
    #[test]
    fn create_pr_via_api_posts_expected_request_shape() {
        let (addr, log_handle) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/up-owner/up-repo/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "up-owner",
            name: "up-repo",
        };
        let pr_spec = PrSpec {
            title: "Update myapp to 1.2.3",
            body: "automated release PR",
            head: "fork-owner:release/v1.2.3",
            base_branch: "develop",
            draft: true,
            update_existing_pr: false,
        };
        let outcome =
            create_pr_via_api_with_env(&upstream, &pr_spec, "tok", "homebrew", &log, &env);
        assert!(outcome.is_none(), "201 is the success path");

        let entries = log_handle.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one POST expected");
        let req = &entries[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/repos/up-owner/up-repo/pulls");
        let payload: serde_json::Value =
            serde_json::from_str(&req.body).expect("request body must be JSON");
        assert_eq!(payload["title"], "Update myapp to 1.2.3");
        assert_eq!(payload["head"], "fork-owner:release/v1.2.3");
        assert_eq!(payload["base"], "develop");
        assert_eq!(payload["body"], "automated release PR");
        assert_eq!(payload["draft"], true);
    }

    /// Pins that a custom (non-default) base branch and `draft=false`
    /// round-trip into the emitted request payload — complementing the
    /// draft=true case above so both branches of the bool are covered in
    /// the request body, not just the return value.
    #[test]
    fn create_pr_via_api_serialises_non_draft_payload() {
        let (addr, log_handle) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/n/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);
        let env = env_with_base(&format!("http://{addr}"));
        let log = quiet_log();
        let upstream = Upstream {
            owner: "o",
            name: "n",
        };
        let pr_spec = PrSpec {
            title: "t",
            body: "b",
            head: "o:feature",
            base_branch: "main",
            draft: false,
            update_existing_pr: false,
        };
        let outcome = create_pr_via_api_with_env(&upstream, &pr_spec, "tok", "label", &log, &env);
        assert!(outcome.is_none());
        let entries = log_handle.lock().unwrap();
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(payload["draft"], false, "draft=false must serialise");
        assert_eq!(payload["base"], "main");
    }

    /// `maybe_submit_pr` renders `body`, `base.owner`, `base.name`, and
    /// `base.branch` through the caller-supplied render closure before they
    /// reach the upstream slug and the API payload. Pins that the PR
    /// request carries the expanded values, never the literal template strings.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn pr_body_and_base_fields_are_rendered_before_submission() {
        let (_tools, _guard) = gh_absent_path();
        let (_origin_url, _bare, work) = init_origin_with_work();
        git_ok(work.path(), &["checkout", "-b", "release/v1.0.0"]);
        write_commit(work.path(), "formula.rb", "formula\n", "add formula");

        // The render closure expands `{{ .ProjectName }}` → `"mytool"`.
        let render = |s: &str| s.replace("{{ .ProjectName }}", "mytool");

        // Responder accepts the PR at the RENDERED upstream slug
        // (`mytool-org/index`), not the literal template slug.
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/mytool-org/index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let log = quiet_log();
        let repo = RepositoryConfig {
            owner: Some("fork-owner".into()),
            name: Some("fork-repo".into()),
            token: Some("ghp_test".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                body: Some("PR for {{ .ProjectName }}".into()),
                base: Some(PullRequestBaseConfig {
                    owner: Some("{{ .ProjectName }}-org".into()),
                    name: Some("index".into()),
                    branch: Some("main".into()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let origin = PrOrigin {
            repo_owner: "fork-owner",
            repo_name: "fork-repo",
            branch_name: "release/v1.0.0",
            update_existing_pr: false,
        };
        let env = MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        let outcome = maybe_submit_pr_with_env(
            work.path(),
            Some(&repo),
            &origin,
            "title",
            "caller body — must be overridden",
            "homebrew",
            &log,
            &render,
            &env,
        );
        assert!(
            outcome.is_none(),
            "201 is the success path; got {outcome:?}"
        );

        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one POST expected");
        let payload: serde_json::Value =
            serde_json::from_str(&entries[0].body).expect("request body must be JSON");
        assert_eq!(
            payload["body"], "PR for mytool",
            "pull_request.body must be rendered, not literal template"
        );
        assert_eq!(
            payload["base"], "main",
            "base.branch must reach the payload as its rendered value"
        );
        // The POST endpoint path itself confirms base.owner + base.name rendered
        // (the responder only matches `/repos/mytool-org/index/pulls`).
        assert!(
            entries[0].path.contains("mytool-org"),
            "upstream owner must be rendered before use in the API slug: {}",
            entries[0].path
        );
        drop(work);
    }
}
