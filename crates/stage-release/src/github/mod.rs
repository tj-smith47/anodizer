//! GitHub release backend.
//!
//! `run_github_backend` is the body of the `ScmTokenType::GitHub` match arm
//! in the dispatcher loop, lifted out of `run.rs` for readability. The
//! per-helper modules (`client`, `rate_limit`, `username`, `assets`) host
//! the GitHub-specific helper functions used by that body.

use std::sync::Arc;

use anodizer_core::config::{CrateConfig, ReleaseConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::jitter_duration;
use anyhow::{Context as _, Result};
use octocrab::repos::releases::MakeLatest;

use crate::release_body::{
    GITHUB_RELEASE_BODY_MAX_CHARS, build_publish_patch_body, build_release_json,
    compose_body_for_mode,
};
use crate::{release_log, resolve_release_repo};

mod assets;
mod client;
mod rate_limit;
mod retry_call;
mod retry_classify;
mod secondary_rate_limit;
mod upload_outcome;

pub(crate) use assets::{delete_release_asset_by_name, find_release_asset_size};
pub(crate) use client::build_octocrab_client;
pub(crate) use rate_limit::check_github_rate_limit_with_env;
pub(crate) use retry_call::{format_retry_warn, is_octocrab_404, retry_octocrab_call};
use secondary_rate_limit::{RetryAfterCapture, secondary_rl_delay};
use upload_outcome::{UploadAttemptOutcome, classify_upload_attempt};

/// Page size used when paginating `GET /repos/{owner}/{repo}/releases`.
///
/// Matches GitHub's per-page maximum so the draft search reaches the
/// answer in the minimum number of round trips. The "fewer than this many
/// results means last page" pagination terminator depends on this value
/// being the same as the `per_page` query parameter sent in
/// [`find_draft_by_name`].
const LIST_RELEASES_PAGE_SIZE: usize = 100;

/// Find a draft release on `{owner}/{repo}` whose `name` field matches
/// `name`, paginating through `GET /repos/{owner}/{repo}/releases` 100
/// results at a time until a match is found or the listing is exhausted.
///
/// GoReleaser parity: mirrors `internal/client/github.go::findDraftRelease`,
/// which searches releases by *name* (not tag) and loops while
/// `resp.NextPage != 0`. There is no artificial page cap so repos with
/// thousands of historical draft releases still locate the target —
/// otherwise the create-release path would 422 on a duplicate tag.
///
/// Each page fetch is wrapped by [`retry_octocrab_call`] so transient
/// 5xx / 429 / transport failures retry according to `policy`; 4xx
/// errors (auth, validation) fast-fail. The retry envelope wraps a single
/// page only: once a page returns OK, the next page is fetched fresh.
///
/// Returns `Ok(Some(release))` when a draft with the matching `name` is
/// found, `Ok(None)` when the listing is exhausted with no match, and
/// `Err(_)` when a non-retryable error surfaces (or every retry has been
/// consumed).
pub(crate) async fn find_draft_by_name(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    name: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Option<octocrab::models::repos::Release>> {
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases?per_page={}&page={}",
            owner, repo, LIST_RELEASES_PAGE_SIZE, page
        );
        let releases: Vec<octocrab::models::repos::Release> =
            retry_octocrab_call(policy, "list releases", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list releases on {}/{} (page {})",
                    owner, repo, page
                )
            })?;
        if let Some(found) = releases
            .iter()
            .find(|r| r.draft && r.name.as_deref() == Some(name))
        {
            return Ok(Some(found.clone()));
        }
        if releases.len() < LIST_RELEASES_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(None)
}

/// Look up the single release that points at `tag` via the GitHub Releases API.
///
/// Returns `Ok(Some(release))` when a release exists for the tag,
/// `Ok(None)` when the tag has no associated release (HTTP 404), and
/// `Err(_)` when any other error surfaces (auth, validation, exhausted retries
/// on 5xx / 429) so the caller sees the real GitHub error rather than silently
/// treating a failed lookup as "no existing release".
async fn find_release_by_tag(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    tag: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
    label: &'static str,
) -> Result<Option<octocrab::models::repos::Release>> {
    let owner = owner.to_string();
    let repo = repo.to_string();
    let tag = tag.to_string();
    let result: Result<octocrab::models::repos::Release, octocrab::Error> =
        retry_octocrab_call(policy, label, retry_after, || {
            let octo = octo.clone();
            let owner = owner.clone();
            let repo = repo.clone();
            let tag = tag.clone();
            async move { octo.repos(&owner, &repo).releases().get_by_tag(&tag).await }
        })
        .await;
    match result {
        Ok(release) => Ok(Some(release)),
        Err(err) if is_octocrab_404(&err) => Ok(None),
        Err(err) => Err(anyhow::Error::new(err)),
    }
}

/// Fetch the names of the assets currently UPLOADED to the published
/// GitHub release for `crate_cfg`'s resolved tag.
///
/// This is the network half of the post-release asset-existence check: the
/// verify-release stage diffs this live, GitHub-stored set against the
/// produced artifact set to catch the partial uploads GitHub silently
/// tolerates. It reuses the hardened release backend's repo-resolution
/// ([`resolve_release_repo`]), tag-resolution
/// ([`resolve_release_tag`](crate::release_body::resolve_release_tag)), and
/// octocrab client/retry path so there is one source of truth for "how do we
/// talk to the GitHub Releases API".
///
/// Returns:
/// - `Ok(Some(names))` — the release exists; `names` are its asset names
///   (empty vec when the release has no assets).
/// - `Ok(None)` — no GitHub repo is configured for the active token type
///   (the verify stage treats this as "not a GitHub release; skip the asset
///   check for this crate" rather than an error).
///
/// Errors when the tag has no release (the publish should have created it —
/// a genuine post-publish defect), when no token is available, or when the
/// GitHub API call fails after retries.
pub async fn fetch_published_asset_names(
    ctx: &Context,
    release_cfg: &ReleaseConfig,
    crate_cfg: &CrateConfig,
) -> Result<Option<Vec<String>>> {
    let Some(repo) = resolve_release_repo(release_cfg, ctx.token_type, ctx)? else {
        return Ok(None);
    };

    let tag = crate::release_body::resolve_release_tag(
        ctx,
        &crate_cfg.tag_template,
        release_cfg.tag.as_deref(),
        &crate_cfg.name,
    )?;

    let token = ctx.options.token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "verify-release: no GitHub token available to fetch the published \
             release's assets (set GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN, or pass --token)"
        )
    })?;

    let github_urls = ctx.config.github_urls.clone();
    let policy = ctx.retry_policy();

    let (octo_raw, retry_after) = build_octocrab_client(&token, &github_urls)?;
    let octo = Arc::new(octo_raw);

    let release = find_release_by_tag(
        &octo,
        &repo.owner,
        &repo.name,
        &tag,
        &policy,
        Some(&retry_after),
        "verify-release fetch published assets",
    )
    .await?;

    match release {
        Some(rel) => Ok(Some(rel.assets.into_iter().map(|a| a.name).collect())),
        None => anyhow::bail!(
            "verify-release: no GitHub release found for tag '{}' on {}/{} — \
             the publish should have created it; this is a post-publish defect",
            tag,
            repo.owner,
            repo.name
        ),
    }
}

/// Number of `GET /releases/{id}` readiness probes attempted before the
/// upload loop starts (see [`wait_for_release_readable`]). The 7 inter-probe
/// sleeps double from [`READINESS_GUARD_BASE_DELAY`] (100 ms) and saturate at
/// [`READINESS_GUARD_MAX_DELAY`] (1500 ms) — 100+200+400+800+1500+1500+1500 ≈
/// 6 s nominal, ~7 s with jitter, leaving headroom under the ~10 s budget so
/// the guard never dominates release wall-clock.
const READINESS_GUARD_ATTEMPTS: u32 = 8;

/// Initial backoff between readiness probes; doubles each slot up to
/// [`READINESS_GUARD_MAX_DELAY`].
const READINESS_GUARD_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Per-slot ceiling for the readiness-probe backoff.
const READINESS_GUARD_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);

/// Guaranteed minimum number of upload attempts for the transient /
/// read-after-write-404 classes, even when the resolved [`RetryPolicy`]
/// caps `max_attempts` at 1 (as stateful modes like `--publish-only` do).
///
/// A 404 from `upload_asset` immediately after the release was created is
/// GitHub's post-create read-after-write replication lag, not a missing
/// release — the asset definitively was not created, so re-issuing the
/// upload is idempotent-safe. Fatal / auth / 422-bail outcomes are
/// unaffected and still fail on the first attempt. Genuinely-missing
/// releases still fail once this floor is exhausted.
const MIN_UPLOAD_TRANSIENT_ATTEMPTS: u32 = 3;

/// Poll `GET /repos/{owner}/{repo}/releases/{id}` until it returns 200,
/// bounded by [`READINESS_GUARD_ATTEMPTS`] with short exponential backoff.
///
/// GitHub serves `POST /releases` from a primary replica but the
/// `GET /releases/{id}` issued by `ReleasesHandler::upload_asset(...).send()`
/// (to read the release's `upload_url`) may hit a replica that has not yet
/// observed the create — a read-after-write lag that surfaces as a transient
/// 404. Because the upload loop fans out in parallel immediately after the
/// create, several of those probes can race the propagation window at once.
///
/// This guard makes the release readable once before any upload starts,
/// shrinking (but not eliminating — replicas lag independently) that window.
/// It runs regardless of the resolved retry policy's `max_attempts`, because
/// it is a consistency guard rather than a flaky-network retry. On persistent
/// failure after the bound it returns `Ok(false)` so the caller proceeds
/// anyway: the per-upload bounded-404 retry is the backstop, and this guard
/// must never introduce a new failure mode of its own.
///
/// Returns `Ok(true)` once the release is readable (immediately on the first
/// probe in the common no-lag case), `Ok(false)` if the bound is exhausted
/// without a 200, and `Err(_)` only for a non-404 hard error (auth, etc.).
async fn wait_for_release_readable(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    release_id: u64,
    log: &StageLogger,
) -> Result<bool> {
    let mut delay = READINESS_GUARD_BASE_DELAY;
    for attempt in 1..=READINESS_GUARD_ATTEMPTS {
        let route = format!("/repos/{owner}/{repo}/releases/{release_id}");
        let result = octo
            .get::<octocrab::models::repos::Release, _, _>(route, None::<&()>)
            .await;
        match result {
            Ok(_) => {
                if attempt > 1 {
                    log.verbose(&format!(
                        "release {release_id} became readable after {attempt} probe(s) \
                         (GitHub post-create propagation lag)"
                    ));
                }
                return Ok(true);
            }
            Err(err) if is_octocrab_404(&err) => {
                if attempt < READINESS_GUARD_ATTEMPTS {
                    tokio::time::sleep(jitter_duration(delay)).await;
                    delay = std::cmp::min(delay * 2, READINESS_GUARD_MAX_DELAY);
                }
            }
            // A non-404 hard error (auth, validation) is not a propagation
            // lag; surface it rather than silently consuming the budget.
            Err(err) => return Err(anyhow::Error::new(err)),
        }
    }
    Ok(false)
}

/// Resolve the upload retry loop's per-iteration locals from a [`RetryPolicy`].
///
/// Returns `(max_upload_attempts, initial_retry_delay, max_retry_delay)` in
/// the order the upload loop binds them. The single point of translation
/// from policy to locals lives here so a future formula change is visible
/// in one place (and so tests can pin the formula against the backend without
/// re-deriving it inline).
///
/// `max_upload_attempts` mirrors [`RetryPolicy::max_attempts`] directly:
/// the `>= 1` invariant is enforced by [`anodizer_core::config::RetryConfig::to_policy`]
/// (clamps `attempts: 0` -> `1`) and `retry_async` / `retry_sync` (defensive
/// clamp at the loop boundary). No additional clamp is needed at the call
/// site.
pub(crate) fn upload_retry_locals(
    policy: &anodizer_core::retry::RetryPolicy,
) -> (u32, std::time::Duration, std::time::Duration) {
    (policy.max_attempts, policy.base_delay, policy.max_delay)
}
// NOTE: A `resolve_github_username` helper used to live alongside this mod
// (search-users API fallback for resolving commit author emails). Upstream
// removed the Search API call entirely in commit 17315a5 (parity item P3),
// leaving only the `users.noreply.github.com` pattern parser, which had no
// callers in anodizer. The whole module was deleted to satisfy the no-
// dead-code anti-pattern rule: a parser with no live call site is dead
// code, so noreply parsing should be re-introduced as a focused helper at
// its actual point of use rather than kept speculatively here.

/// Runtime / context infrastructure for [`run_github_backend`].
///
/// Bundles the four "ambient" handles every backend call needs: the
/// shared tokio runtime, the global anodizer [`Context`], the per-stage
/// logger, and the resolved GitHub token. Pulling them into a struct
/// drains four positional arguments off the call site.
pub(crate) struct BackendEnv<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub ctx: &'a Context,
    pub log: &'a StageLogger,
    pub token: &'a Option<String>,
}

/// Per-release attributes consumed by [`run_github_backend`].
///
/// Mirrors `GitlabReleaseSpec` / `GiteaReleaseSpec` from the sibling
/// `gitlab.rs` / `gitea.rs` backends. Field names line up with
/// [`crate::release_body::ReleaseJsonSpec`] so the `build_release_json`
/// call site is a near-direct field forward.
#[derive(Clone, Copy)]
pub(crate) struct GithubReleaseSpec<'a> {
    pub tag: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub mode: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub make_latest: &'a Option<MakeLatest>,
    pub target_commitish: &'a Option<String>,
    pub discussion_category: &'a Option<String>,
}

/// Cluster controlling upload + retention semantics for [`run_github_backend`].
#[derive(Clone)]
pub(crate) struct UploadOpts {
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub replace_existing_artifacts: bool,
    pub use_existing_draft: bool,
    /// `--resume-release`: bypass the leftover-assets pre-check so the
    /// upload loop runs against an existing release left by a prior failed
    /// attempt.
    pub resume_release: bool,
    /// Nightly retention: keep the N newest nightly releases (matched by the
    /// rendered nightly name) and delete the rest before creating the new
    /// one, including the git tags anodizer created for them. `keep_last: 1`
    /// is the rolling-single-release case (`keep_single_release`); `None`
    /// disables the sweep. Operates on [`Self::publish_repo_override`] when
    /// set. Resolution of the legacy `keep_single_release` alias vs the
    /// `retention:` block happens upstream in
    /// [`anodizer_core::config::NightlyConfig::resolved_keep_last`], so this
    /// field is the single source of truth for the backend.
    pub retention_keep_last: Option<usize>,
    /// Nightly `publish_repo`: redirect the release create, asset upload, AND
    /// retention delete calls to a DIFFERENT `(owner, repo)` than the source
    /// repo resolved from `release.github`. `None` = source repo, unchanged.
    pub publish_repo_override: Option<(String, String)>,
}

/// Outcome for the upload-asset 422 `already_exists` decision branch.
/// Extracted from the body of [`run_github_backend`] so the logic can be
/// unit-tested without standing up a fake octocrab.
///
/// Mirrors GoReleaser `internal/client/github.go:734-744`:
///
/// ```text
/// if resp.StatusCode == http.StatusUnprocessableEntity {
///     if !ctx.Config.Release.ReplaceExistingArtifacts {
///         return retryx.Unrecoverable(err)
///     }
///     // delete + retry
/// }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AlreadyExistsAction {
    /// Local + remote bytes match: treat as a no-op (idempotency); a
    /// prior attempt in this same release already uploaded the file.
    SkipIdempotent,
    /// `replace_existing_artifacts: false` and bytes differ: bail with
    /// the conflict instead of overwriting.
    BailReplaceForbidden,
    /// Different bytes and the user opted in via
    /// `replace_existing_artifacts: true`: delete the stale asset and
    /// retry the upload.
    DeleteAndRetry,
}

/// Check whether an existing release's assets block a retry when
/// `replace_existing_artifacts` is false. Returns the list of asset names
/// that would conflict, or `None` when uploads may proceed.
///
/// Pure function so the pre-check logic can be unit-tested without I/O.
/// Returns `None` (uploads proceed) when ANY of:
///   - `skip_upload` is true (nothing will be uploaded),
///   - `resume_release` is true (the user explicitly opted into continuing
///     into a leftover release via `--resume-release`),
///   - `replace_existing_artifacts` is true (overwrites are permitted), or
///   - no assets exist on the release yet.
pub(crate) fn check_existing_assets_block_upload(
    skip_upload: bool,
    resume_release: bool,
    replace_existing_artifacts: bool,
    existing_asset_names: &[&str],
) -> Option<Vec<String>> {
    if skip_upload
        || resume_release
        || replace_existing_artifacts
        || existing_asset_names.is_empty()
    {
        return None;
    }
    Some(existing_asset_names.iter().map(|s| s.to_string()).collect())
}

/// Decide what to do when the GitHub upload-asset API returns
/// `422 already_exists`. Pure function so the (re-)introduced
/// `replace_existing_artifacts: false` guard can be tested without I/O.
pub(crate) fn classify_already_exists(
    replace_existing_artifacts: bool,
    remote_size: Option<u64>,
    local_size: u64,
) -> AlreadyExistsAction {
    // Idempotency check first: bytes that already match the local
    // artifact aren't an "overwrite", so the user's
    // `replace_existing_artifacts: false` does NOT block this path.
    if remote_size == Some(local_size) {
        return AlreadyExistsAction::SkipIdempotent;
    }
    if !replace_existing_artifacts {
        return AlreadyExistsAction::BailReplaceForbidden;
    }
    AlreadyExistsAction::DeleteAndRetry
}

/// Decide which nightly releases to prune so that — after the about-to-be-created
/// release is added — exactly `keep_last` nightly releases survive.
///
/// `releases` is the set of existing releases (`(id, tag)`) whose `name` matches
/// the nightly release name. They are sorted newest-first internally by release
/// `id` descending — monotonic with creation order on a single repo — so
/// correctness does not depend on the order GitHub returns them. Because the new
/// release will become the newest of the kept set, the prune target is "every
/// release beyond the newest `keep_last - 1`": that leaves `keep_last - 1` old
/// releases plus the new one = `keep_last`.
///
/// For `keep_last = 1` this returns ALL existing nightly releases — the rolling
/// single-release semantics (only the just-created release survives). This is the
/// single function both the `keep_single_release` alias and `retention.keep_last`
/// route through; there is no parallel single-delete path.
///
/// Pure (no I/O) so the keep/delete arithmetic is unit-testable without octocrab.
pub(crate) fn nightly_releases_to_prune(
    releases: &[(u64, String)],
    keep_last: usize,
) -> Vec<(u64, String)> {
    let keep_last = keep_last.max(1);
    // Sort newest-first by id descending so the keep/prune split is correct
    // regardless of the API response order.
    let mut sorted = releases.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.0));
    // The new release occupies one of the kept slots, so retain `keep_last - 1`
    // of the existing (newest-first) set and prune the remainder.
    sorted.into_iter().skip(keep_last - 1).collect()
}

/// List all releases on `{owner}/{repo}` whose `name` field equals `name`,
/// returning `(id, tag_name)` pairs in the order GitHub returns them
/// (newest-first — the Releases API lists by `created_at` descending).
///
/// Used by the nightly retention sweep to enumerate prior nightly releases
/// sharing the rendered nightly release name (the per-build differentiator
/// lives in the TAG, not the name, so the name is the stable matcher).
async fn list_releases_by_name(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    name: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases?per_page={}&page={}",
            owner, repo, LIST_RELEASES_PAGE_SIZE, page
        );
        let releases: Vec<octocrab::models::repos::Release> =
            retry_octocrab_call(policy, "list releases (retention)", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list releases on {}/{} for retention (page {})",
                    owner, repo, page
                )
            })?;
        let page_len = releases.len();
        for r in releases {
            if r.name.as_deref() == Some(name) {
                out.push((r.id.into_inner(), r.tag_name));
            }
        }
        if page_len < LIST_RELEASES_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(out)
}

/// Run the GitHub release backend for one crate.
///
/// Returns:
/// - `Ok(Some((release_html_url, download_base, owner, repo)))` on success.
/// - `Ok(None)` when no `release.github` config is present for the crate
///   (callers should `continue` the outer loop with a warning already logged).
pub(crate) fn run_github_backend(
    env: &BackendEnv<'_>,
    crate_cfg: &CrateConfig,
    release_cfg: &ReleaseConfig,
    spec: &GithubReleaseSpec<'_>,
    upload_opts: &UploadOpts,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
) -> Result<Option<(String, String, String, String)>> {
    let BackendEnv {
        rt,
        ctx,
        log,
        token,
    } = *env;
    let GithubReleaseSpec {
        tag,
        name: release_name,
        body: release_body,
        mode: release_mode,
        draft,
        prerelease,
        make_latest,
        target_commitish,
        discussion_category: discussion_category_name,
    } = *spec;
    let UploadOpts {
        skip_upload,
        replace_existing_draft,
        replace_existing_artifacts,
        use_existing_draft,
        resume_release,
        retention_keep_last,
        publish_repo_override,
    } = upload_opts;
    let skip_upload = *skip_upload;
    let replace_existing_draft = *replace_existing_draft;
    let replace_existing_artifacts = *replace_existing_artifacts;
    let use_existing_draft = *use_existing_draft;
    let resume_release = *resume_release;
    let retention_keep_last = *retention_keep_last;
    let github = match resolve_release_repo(release_cfg, ctx.token_type, ctx)? {
        Some(r) => r,
        None => {
            log.warn(&format!(
                "no github config for crate '{}', skipping",
                crate_cfg.name
            ));
            return Ok(None);
        }
    };
    // Nightly `publish_repo`: redirect EVERY octocrab call (draft search,
    // release create/update, asset upload, retention delete, html_url) to the
    // override repo by rebinding `github` here. Downstream code reads only
    // `github.owner` / `github.name`, so this single rebind threads the
    // override through the entire backend without forking any path. The
    // active token is assumed to have write access to the override repo.
    let github = match publish_repo_override {
        Some((owner, name)) => anodizer_core::config::ScmRepoConfig {
            owner: owner.clone(),
            name: name.clone(),
        },
        None => github,
    };

    // Require a token for real API calls.
    let token_str = match token {
        Some(t) => t.clone(),
        None => {
            anyhow::bail!(
                "release: no GitHub token available (set GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN, or pass --token)"
            );
        }
    };

    // Extract github_urls config for GitHub Enterprise support.
    let github_urls = ctx.config.github_urls.clone();
    // Default download URL to "https://github.com" (matches GoReleaser's DefaultGitHubDownloadURL).
    let gh_download_base = github_urls
        .as_ref()
        .and_then(|u| u.download.clone())
        .unwrap_or_else(|| "https://github.com".to_string());

    // Resolve the user-configurable retry policy once. Every retriable
    // octocrab call site below threads this through the shared
    // `retry_octocrab_call` helper so a `retry:` block in the project config
    // controls every transient-failure path uniformly.
    let policy = ctx.retry_policy();

    // Resolve the env source as an `Arc` so spawned upload tasks can
    // clone-and-move it into their `'static` futures, while in-block
    // helpers read through the borrowed `&dyn` form.
    let env_source_arc = ctx.env_source_arc();
    let env_source: &dyn anodizer_core::EnvSource = env_source_arc.as_ref();

    // Build the octocrab instance and perform async API calls inside a
    // dedicated tokio runtime (the Stage trait is synchronous).
    let url = rt.block_on(async {
        // Wrap octo in Arc up front so the retry-wrapped closures (and the
        // parallel upload tasks downstream) can `Clone` a fresh handle per
        // attempt without moving the original.
        let (octo_raw, retry_after_capture) = build_octocrab_client(&token_str, &github_urls)?;
        let octo = Arc::new(octo_raw);
        let rate_limit_client = reqwest::Client::new();

        // Proactive rate limit check before draft search/release operations.
        check_github_rate_limit_with_env(&rate_limit_client, &token_str, 10, env_source).await;

        // Cleanup is unconditional on the NEW release's draft flag: a leftover
        // draft is stale state to remove whether we are about to publish or
        // re-draft. `find_draft_by_name` only ever matches `r.draft` releases,
        // so deleting what it returns can never touch a published/live
        // release — gating on `draft` would only leave the stale draft in
        // place when publishing (`draft: false`), and that draft's id later
        // goes 404 on the upload_url read, killing the publish.
        if replace_existing_draft
            && let Some(existing) =
                find_draft_by_name(&octo, &github.owner, &github.name, release_name, &policy, Some(&retry_after_capture))
                    .await?
        {
            log.status(&format!(
                "replacing existing draft release '{}' (id={})",
                release_name, existing.id
            ));
            let existing_id = existing.id.into_inner();
            let owner = github.owner.clone();
            let repo = github.name.clone();
            retry_octocrab_call(&policy, "delete release", Some(&retry_after_capture), || {
                let octo = octo.clone();
                let owner = owner.clone();
                let repo = repo.clone();
                async move {
                    octo.repos(&owner, &repo)
                        .releases()
                        .delete(existing_id)
                        .await
                }
            })
            .await
            .with_context(|| {
                format!(
                    "release: delete existing draft release '{}' on {}/{}",
                    release_name, github.owner, github.name
                )
            })?;
        }

        // Handle use_existing_draft: look for an existing draft release
        // with the same NAME and update it instead of creating a new one.
        let existing_draft = if use_existing_draft {
            match find_draft_by_name(&octo, &github.owner, &github.name, release_name, &policy, Some(&retry_after_capture))
                .await?
            {
                Some(existing) => {
                    log.status(&format!(
                        "reusing existing draft release '{}' (id={})",
                        release_name, existing.id
                    ));
                    Some(existing)
                }
                None => None,
            }
        } else {
            None
        };

        // Nightly retention sweep: keep the N newest nightly releases (matched
        // by the rendered nightly release name) and delete the rest before
        // creating the new one, so after this run exactly `keep_last` nightly
        // releases survive. `keep_last: 1` is the rolling-single-release case
        // (the `keep_single_release` alias resolves to it upstream); larger N
        // keeps N (e.g. nushell keeps 10). All route through the same prune
        // arithmetic ([`nightly_releases_to_prune`]) — no parallel path.
        //
        // Skipped when an existing-draft reuse is in play (the draft IS the
        // release we'll PATCH).
        //
        // Tag handling: each pruned release's git ref is deleted too, EXCEPT
        // the tag we are about to (re)create — leaving that ref intact lets the
        // create POST below reuse it instead of dangling. For distinct-tag
        // schemes (nushell `…-nightly.<build>+<sha>`) every pruned tag differs
        // from the current one, so all stale refs are cleaned up.
        if let Some(keep_last) = retention_keep_last
            && existing_draft.is_none()
        {
            let existing = list_releases_by_name(
                &octo,
                &github.owner,
                &github.name,
                release_name,
                &policy,
                Some(&retry_after_capture),
            )
            .await?;
            let to_prune = nightly_releases_to_prune(&existing, keep_last);
            for (rel_id, rel_tag) in to_prune {
                log.status(&format!(
                    "nightly retention (keep_last={keep_last}): deleting prior release '{release_name}' (id={rel_id}, tag='{rel_tag}')"
                ));
                let delete_result = retry_octocrab_call(&policy, "delete release (retention)", Some(&retry_after_capture), || {
                    let octo = octo.clone();
                    let owner = github.owner.clone();
                    let repo = github.name.clone();
                    async move {
                        octo.repos(&owner, &repo)
                            .releases()
                            .delete(rel_id)
                            .await
                    }
                })
                .await;
                match delete_result {
                    Ok(()) => {}
                    Err(ref err) if is_octocrab_404(err) => {
                        // A concurrent process already removed the release; the
                        // post-condition (release gone) is satisfied.
                        log.status(&format!(
                            "nightly retention: release '{release_name}' (id={rel_id}) already deleted by concurrent process"
                        ));
                    }
                    Err(err) => {
                        return Err(anyhow::Error::new(err)).with_context(|| {
                            format!(
                                "release: delete prior nightly release (id={rel_id}) on {}/{}",
                                github.owner, github.name
                            )
                        });
                    }
                }
                // Delete the pruned release's git tag too, unless it is the tag
                // we're about to (re)create (which the create POST reuses).
                if rel_tag != tag && !rel_tag.is_empty() {
                    let tag_route = format!(
                        "/repos/{}/{}/git/refs/tags/{}",
                        github.owner, github.name, rel_tag
                    );
                    let tag_delete: std::result::Result<(), octocrab::Error> =
                        retry_octocrab_call(&policy, "delete tag (retention)", Some(&retry_after_capture), || {
                            let octo = octo.clone();
                            let route = tag_route.clone();
                            async move {
                                octo._delete(route, None::<&()>).await.map(|_| ())
                            }
                        })
                        .await;
                    match tag_delete {
                        Ok(()) => {}
                        // Already-absent tag is success (the prune post-condition).
                        Err(ref err) if is_octocrab_404(err) => {}
                        Err(err) => {
                            // A failed tag delete is non-fatal: the release (the
                            // user-visible artifact) is already gone. Warn and
                            // continue rather than abort the whole publish.
                            log.warn(&format!(
                                "nightly retention: failed to delete stale tag '{rel_tag}' on {}/{}: {err}",
                                github.owner, github.name
                            ));
                        }
                    }
                }
            }
        }

        // When updating an existing release, apply mode-based body composition.
        // Also track any existing release found by tag so we can PATCH it
        // instead of POSTing a new one (which would 422 on duplicate tags).
        let (final_body, existing_by_tag) = if let Some(ref existing) = existing_draft {
            let existing_body = existing.body.as_deref();
            (
                compose_body_for_mode(release_mode, existing_body, release_body),
                None,
            )
        } else {
            // For new releases, check if a release exists for mode != "replace".
            if release_mode != "replace" {
                check_github_rate_limit_with_env(&rate_limit_client, &token_str, 10, env_source)
                    .await;
                // Look up an existing release by tag through the shared retry
                // helper so a transient 5xx / 429 / transport failure retries
                // instead of mis-classifying as "no existing release", which
                // would fall through to the create-release POST and surface
                // GitHub's confusing "tag already exists" 422.
                //
                // Error handling: a real 404 means "no release for that tag"
                // and yields `(release_body, None)` so the create-release POST
                // runs. Any other error (auth, validation, exhausted retries
                // on 5xx) propagates with `with_context` so the user sees the
                // real GitHub error instead of a downstream 422.
                let existing = find_release_by_tag(
                    &octo,
                    &github.owner,
                    &github.name,
                    tag,
                    &policy,
                    Some(&retry_after_capture),
                    "get release by tag",
                )
                .await
                .with_context(|| {
                    format!(
                        "release: look up existing release by tag '{}' on {}/{}",
                        tag, github.owner, github.name
                    )
                })?;
                match existing {
                    Some(existing) => {
                        let existing_body = existing.body.as_deref();
                        let body =
                            compose_body_for_mode(release_mode, existing_body, release_body);
                        (body, Some(existing))
                    }
                    None => (release_body.to_string(), None),
                }
            } else {
                (release_body.to_string(), None)
            }
        };

        // Leftover-assets pre-check: if a prior failed attempt already created
        // the release and uploaded some assets, and the user hasn't opted into
        // overwriting (replace_existing_artifacts: false) nor into resuming
        // (--resume-release), bail early with a clear message instead of
        // letting the upload loop hit 422 already_exists per-asset.
        if let Some(ref existing) = existing_by_tag {
            let asset_names: Vec<&str> =
                existing.assets.iter().map(|a| a.name.as_str()).collect();
            if let Some(conflicting) = check_existing_assets_block_upload(
                skip_upload,
                resume_release,
                replace_existing_artifacts,
                &asset_names,
            ) {
                anyhow::bail!(
                    "release: GitHub release for tag '{}' already exists with {} asset(s) ({}) \
                     left by a prior failed attempt. To recover, pass one of:\n\
                     \x20 • --resume-release  (continue into the existing release; assumes its \
                     assets are correct), or\n\
                     \x20 • --replace-existing  (overwrite the assets with the current build), or\n\
                     \x20 • set release.replace_existing_artifacts: true in config, or\n\
                     \x20 • delete the existing release manually and retry.",
                    tag,
                    conflicting.len(),
                    conflicting.join(", ")
                );
            }
        }

        // Create or update the release. We use raw API calls for all paths
        // to support target_commitish and discussion_category_name, which
        // are not fully exposed by octocrab's builder API.
        //
        // Draft-then-publish: always create as draft first so users never
        // see a release with missing artifacts. After all uploads succeed,
        // we PATCH draft=false if the user wanted a non-draft release.
        let user_wants_draft = draft;
        // GitHub ignores discussion_category_name on draft releases and
        // make_latest is meaningless until publish. Send them only in the
        // un-draft PATCH (below) to match GoReleaser behaviour.
        if final_body.len() > GITHUB_RELEASE_BODY_MAX_CHARS {
            log.warn(&format!(
                "release body ({} chars) exceeds GitHub limit ({}); truncating",
                final_body.len(),
                GITHUB_RELEASE_BODY_MAX_CHARS,
            ));
        }
        let json_body = build_release_json(&crate::release_body::ReleaseJsonSpec {
            tag,
            name: release_name,
            body: &final_body,
            draft: true, // always create as draft first
            prerelease_flag: prerelease,
            make_latest: &None, // applied at the publish PATCH below
            target_commitish,
            discussion_category: &None, // applied at the publish PATCH below
        });

        // Rate limit check before release create/update API call.
        check_github_rate_limit_with_env(&rate_limit_client, &token_str, 10, env_source).await;

        // True when this invocation merely re-touches a release that is
        // already live (not a draft) — the publish-pipeline pass that runs
        // after the release stage already created and published it. In that
        // case the PATCH is idempotent and the create/publish log lines would
        // be a confusing duplicate, so they are replaced by a single
        // `release already live` line below.
        let mut retouch_live = false;
        let release = if let Some(ref existing) = existing_draft {
            // Update the existing draft release via PATCH.
            let route = format!(
                "/repos/{}/{}/releases/{}",
                github.owner, github.name, existing.id
            );
            retry_octocrab_call(&policy, "update draft release", Some(&retry_after_capture), || {
                let route = route.clone();
                let body = json_body.clone();
                let octo = octo.clone();
                async move {
                    octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&body))
                        .await
                }
            })
            .await
            .with_context(|| {
                format!(
                    "release: update existing draft release '{}' on {}/{}",
                    tag, github.owner, github.name
                )
            })?
        } else if let Some(ref existing) = existing_by_tag {
            // An existing release was found by tag (append/prepend/keep-existing
            // mode). PATCH it instead of POSTing a new one, which would cause
            // a 422 "tag already exists" error from GitHub.
            if existing.draft {
                log.status(&format!(
                    "updating existing release '{}' (id={}, mode={})",
                    release_name, existing.id, release_mode
                ));
            } else {
                retouch_live = true;
                log.status(&format!(
                    "release already live: {} (id={}, mode={})",
                    release_name, existing.id, release_mode
                ));
            }
            let route = format!(
                "/repos/{}/{}/releases/{}",
                github.owner, github.name, existing.id
            );
            // preserve the existing
            // release's draft state on PATCH. Our default json_body is
            // built with `draft=true` for the create path; when updating
            // an existing release we must not flip it back to draft.
            let mut patch_body = json_body.clone();
            if let Some(obj) = patch_body.as_object_mut() {
                obj.insert(
                    "draft".to_string(),
                    serde_json::Value::Bool(existing.draft),
                );
            }
            retry_octocrab_call(&policy, "update existing release", Some(&retry_after_capture), || {
                let route = route.clone();
                let body = patch_body.clone();
                let octo = octo.clone();
                async move {
                    octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&body))
                        .await
                }
            })
            .await
            .with_context(|| {
                format!(
                    "release: update existing release '{}' on {}/{}",
                    tag, github.owner, github.name
                )
            })?
        } else {
            // Create a new release via POST.
            let route = format!("/repos/{}/{}/releases", github.owner, github.name);
            retry_octocrab_call(&policy, "create release", Some(&retry_after_capture), || {
                let route = route.clone();
                let body = json_body.clone();
                let octo = octo.clone();
                async move {
                    octo.post::<_, octocrab::models::repos::Release>(route, Some(&body))
                        .await
                }
            })
            .await
            .with_context(|| {
                format!(
                    "release: create GitHub release '{}' on {}/{}",
                    tag, github.owner, github.name
                )
            })?
        };

        if !retouch_live {
            log.status(&format!(
                "created GitHub Release '{}' (id={}) on {}/{}",
                release_name, release.id, github.owner, github.name
            ));
        }

        // Construct the public release URL deterministically from
        // owner/repo/tag, matching GoReleaser `internal/pipe/release/scm.go:26-33`.
        // The GitHub API's `html_url` for draft releases is
        // `.../releases/tag/untagged-<sha>` (because no git tag exists
        // yet), and keeping that URL makes announcement emails /
        // publishers emit broken links that 404 after the draft is
        // published.
        let html_url = format!(
            "{}/{}/{}/releases/tag/{}",
            gh_download_base.trim_end_matches('/'),
            github.owner,
            github.name,
            tag,
        );
        let release_id_raw = release.id.into_inner();

        // Upload artifacts (unless skip_upload is set), with bounded
        // parallelism using a semaphore (context's parallelism setting,
        // minimum 1).
        if skip_upload {
            log.status("skip_upload is set, skipping artifact uploads");
        } else {
            // Upload concurrency cap: env > config > default (4).
            // Separate from ctx.options.parallelism (which governs build
            // concurrency) so large artifact lists don't trigger GitHub's
            // secondary rate limit by blasting 100+ uploads simultaneously.
            let upload_concurrency: usize = ctx
                .env_var("ANODIZER_GITHUB_UPLOAD_CONCURRENCY")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|&n| n > 0)
                .or_else(|| {
                    release_cfg
                        .upload_concurrency
                        .filter(|&n| n > 0)
                })
                .unwrap_or(4) as usize;
            let semaphore = Arc::new(tokio::sync::Semaphore::new(upload_concurrency));
            let gh_owner = github.owner.clone();
            let gh_name = github.name.clone();
            let tag_for_upload = tag.to_string();

            // Prepare the list of uploadable entries (error on missing files).
            let mut missing_files = Vec::new();
            let prepared_entries: Vec<(std::path::PathBuf, String)> = artifact_entries
                .iter()
                .filter_map(|(path, custom_name)| {
                    if !path.exists() {
                        missing_files.push(path.display().to_string());
                        return None;
                    }
                    let file_name = if let Some(name) = custom_name {
                        name.clone()
                    } else {
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "artifact".to_string())
                    };
                    Some((path.clone(), file_name))
                })
                .collect();

            if !missing_files.is_empty() {
                anyhow::bail!(
                    "the following artifact files are missing:\n  {}",
                    missing_files.join("\n  ")
                );
            }

            // Readiness guard: octocrab's `upload_asset(...).send()` issues a
            // `GET /releases/{id}` (to read `upload_url`) before each upload
            // POST. Right after the create POST those reads can hit a GitHub
            // replica that has not yet observed the new release, returning a
            // transient 404. Because uploads fan out in parallel, several of
            // those reads race the propagation window simultaneously. Block
            // once here until the release is readable so the common case never
            // enters that race; a persistent miss returns `Ok(false)` and the
            // loop proceeds (the per-upload bounded-404 retry below is the
            // backstop).
            // Only run when there is at least one asset to upload — an empty
            // upload set issues no `GET`, so the guard would be pure overhead.
            if !prepared_entries.is_empty() {
                wait_for_release_readable(&octo, &github.owner, &github.name, release_id_raw, log)
                    .await?;
            }

            let mut join_set = tokio::task::JoinSet::new();

            for (path, file_name) in prepared_entries {
                let sem = semaphore.clone();
                let octo = octo.clone();
                let gh_owner = gh_owner.clone();
                let gh_name = gh_name.clone();
                let tag_c = tag_for_upload.clone();
                let token_for_rate_limit = token_str.clone();
                let retry_after_for_upload = retry_after_capture.clone();
                let env_for_upload = Arc::clone(&env_source_arc);
                // `policy` is `Copy`; the spawned async move borrows it
                // implicitly into the future. Bind a fresh copy per
                // iteration so the for-loop body still owns `policy`
                // for the next iteration.
                let policy_for_upload = policy;

                join_set.spawn(async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                    // Immutable-releases policy: never pre-emptively delete a
                    // published asset. The 422 already_exists arm below probes
                    // the asset's size and dispatches Skip / Bail /
                    // DeleteAndRetry via classify_already_exists — that is the
                    // only delete site for an already-published asset.

                    // Retry parameters come from `ctx.config.retry` (resolved
                    // into `policy` above): `attempts` caps the loop,
                    // `delay`/`max_delay` shape the exponential backoff. The
                    // loop body remains bespoke (resume-stream + 422
                    // already-exists handling); only the knobs are
                    // user-configurable. The `>= 1` clamp lives at
                    // `RetryConfig::to_policy` (see `RetryPolicy::max_attempts`
                    // rustdoc); no additional clamp is needed here.
                    let (configured_attempts, initial_retry_delay, max_retry_delay) =
                        upload_retry_locals(&policy);
                    // The transient / read-after-write-404 classes get a
                    // guaranteed floor of attempts even when stateful modes
                    // (e.g. `--publish-only`) resolve `max_attempts` to 1:
                    // a single post-create 404 is recoverable and must not
                    // kill the whole release. The Fatal / 422-bail arms still
                    // fast-fail regardless of this floor.
                    let max_upload_attempts =
                        std::cmp::max(configured_attempts, MIN_UPLOAD_TRANSIENT_ATTEMPTS);

                    let mut last_err: Option<anyhow::Error> = None;
                    // One-shot overwrite guard: once we've successfully deleted a
                    // stale asset and the upload *still* hits `already_exists`, give
                    // up gracefully instead of looping. This happens when GitHub's
                    // release-asset delete is eventually consistent: our delete
                    // returns Ok immediately but the subsequent upload still sees
                    // the stale asset for a short window. Rather than burn 10
                    // retries (and ultimately fail the whole release), accept the
                    // stale bytes and move on.
                    let mut overwrite_attempted = false;
                    for attempt in 1..=max_upload_attempts {
                        let data = std::fs::read(&path).with_context(|| {
                            format!("release: read artifact {}", path.display())
                        })?;
                        let local_size = data.len() as u64;

                        let result = octo
                            .repos(&gh_owner, &gh_name)
                            .releases()
                            .upload_asset(release_id_raw, &file_name, data.into())
                            .send()
                            .await;
                        let outcome = classify_upload_attempt(&result);
                        match outcome {
                            UploadAttemptOutcome::Success => {
                                last_err = None;
                                break;
                            }
                            UploadAttemptOutcome::AlreadyExists => {
                                let err = result.expect_err(
                                    "AlreadyExists outcome guarantees Err variant",
                                );
                                // If a prior attempt successfully deleted the stale
                                // asset and the upload *still* surfaces
                                // already_exists, give up rather than looping until
                                // max_upload_attempts exhausts. The re-appearing
                                // asset is typically a GitHub backend
                                // eventual-consistency window after the prior
                                // successful delete; retrying does not help.
                                if overwrite_attempted {
                                    release_log().warn(&format!(
                                        "existing asset '{file_name}' on release '{tag_c}' \
                                         reappeared after delete+retry; \
                                         skipping, stale asset kept"
                                    ));
                                    last_err = None;
                                    break;
                                }

                                // Probe the remote asset's size to distinguish
                                // "same bytes uploaded earlier" (idempotent no-op)
                                // from "different bytes, user opted out of
                                // overwrites" (unrecoverable). The classifier
                                // [`classify_already_exists`] encodes the
                                // GR-aligned 422 decision rule
                                // (`internal/client/github.go:734-744`).
                                let remote_size = find_release_asset_size(
                                    &octo,
                                    &gh_owner,
                                    &gh_name,
                                    release_id_raw,
                                    &file_name,
                                    &policy_for_upload,
                                    Some(&retry_after_for_upload),
                                )
                                .await
                                .with_context(|| {
                                    format!(
                                        "release: look up existing asset '{}' on release '{}'",
                                        file_name, tag_c
                                    )
                                })?;

                                match classify_already_exists(
                                    replace_existing_artifacts,
                                    remote_size,
                                    local_size,
                                ) {
                                    AlreadyExistsAction::SkipIdempotent => {
                                        // A prior attempt in this same release
                                        // already uploaded byte-identical
                                        // content. Pure no-op, regardless of
                                        // `replace_existing_artifacts`.
                                        last_err = None;
                                        break;
                                    }
                                    AlreadyExistsAction::BailReplaceForbidden => {
                                        // User explicitly set
                                        // `replace_existing_artifacts: false`
                                        // and the bytes differ: surface the
                                        // conflict rather than overwriting.
                                        // Mirrors GR's `Unrecoverable(err)`
                                        // return at `github.go:736`.
                                        return Err(anyhow::anyhow!(err)).with_context(|| {
                                            format!(
                                                "release: artifact '{}' already exists on release '{}' \
                                                 with different bytes and `replace_existing_artifacts: false` \
                                                 forbids overwriting (set \
                                                 `release.replace_existing_artifacts: true` \
                                                 to permit overwrites)",
                                                file_name, tag_c
                                            )
                                        });
                                    }
                                    AlreadyExistsAction::DeleteAndRetry => {
                                        // Fall through to the delete-retry
                                        // arm below (user opted in via
                                        // `replace_existing_artifacts: true`).
                                    }
                                }

                                // Size mismatch + user opted in via
                                // `replace_existing_artifacts: true`: delete
                                // the stale asset and retry. If the delete
                                // itself fails (perms, asset disappeared
                                // mid-flight, etc.), warn and treat the
                                // upload as skipped: a stale asset is
                                // better than aborting the release.
                                match delete_release_asset_by_name(
                                    &octo,
                                    &gh_owner,
                                    &gh_name,
                                    release_id_raw,
                                    &file_name,
                                    &policy_for_upload,
                                    Some(&retry_after_for_upload),
                                )
                                .await
                                {
                                    Ok(_) => {
                                        overwrite_attempted = true;
                                        last_err = Some(anyhow::anyhow!(err));
                                        if attempt < max_upload_attempts {
                                            let base = std::cmp::min(
                                                initial_retry_delay * 2u32.pow(attempt - 1),
                                                max_retry_delay,
                                            );
                                            tokio::time::sleep(jitter_duration(base)).await;
                                        }
                                        continue;
                                    }
                                    Err(del_err) => {
                                        release_log().warn(&format!(
                                            "could not overwrite existing asset '{file_name}' on release '{tag_c}' \
                                             (size mismatch and delete failed: {del_err}); skipping, stale asset kept"
                                        ));
                                        last_err = None;
                                        break;
                                    }
                                }
                            }
                            UploadAttemptOutcome::SecondaryRateLimited => {
                                // Secondary rate-limit (403/429 with GitHub's
                                // secondary-RL body): sleep the dedicated RL
                                // delay (with ±20 % jitter) before retrying. Do
                                // NOT fall through to the primary
                                // `check_github_rate_limit` path — secondary
                                // limits are transient burst guards, not quota
                                // exhaustion.
                                let err = result.expect_err(
                                    "SecondaryRateLimited outcome guarantees Err variant",
                                );
                                let delay = jitter_duration(secondary_rl_delay(Some(&retry_after_for_upload)));
                                release_log().warn(&format!(
                                    "release: upload of '{file_name}' hit GitHub secondary \
                                     rate limit; sleeping {:.1}s before retry \
                                     (attempt {attempt}/{})",
                                    delay.as_secs_f64(),
                                    max_upload_attempts,
                                ));
                                if attempt < max_upload_attempts {
                                    tokio::time::sleep(delay).await;
                                }
                                last_err = Some(anyhow::anyhow!(err));
                                continue;
                            }
                            UploadAttemptOutcome::PrimaryRateLimited => {
                                // Primary rate-limit (403/429 without the
                                // secondary-RL body): probe `/rate_limit` and
                                // sleep until quota resets.
                                let err = result.expect_err(
                                    "PrimaryRateLimited outcome guarantees Err variant",
                                );
                                release_log().status(&format!(
                                    "rate limited on upload of '{file_name}', checking rate limits..."
                                ));
                                check_github_rate_limit_with_env(
                                    &reqwest::Client::new(),
                                    &token_for_rate_limit,
                                    100,
                                    env_for_upload.as_ref(),
                                )
                                .await;
                                last_err = Some(anyhow::anyhow!(err));
                                continue;
                            }
                            UploadAttemptOutcome::NotFound => {
                                // octocrab's `upload_asset(...).send()` does a
                                // `GET /releases/{id}` (to read `upload_url`)
                                // before the POST; right after the create that
                                // read can hit a GitHub replica lagging the
                                // create, yielding a transient 404. The asset
                                // was definitively not created, so retrying is
                                // idempotent-safe. Bounded by
                                // `max_upload_attempts` (floored at
                                // MIN_UPLOAD_TRANSIENT_ATTEMPTS) so a genuinely
                                // missing release still fails once exhausted.
                                let err = result.expect_err(
                                    "NotFound outcome guarantees Err variant",
                                );
                                let label = format!("upload of '{file_name}'");
                                // NotFound is by construction a 404, so the
                                // status is a literal here rather than extracted
                                // from the error as the TransientRetry arm does.
                                release_log().warn(&format_retry_warn(
                                    &label,
                                    attempt,
                                    max_upload_attempts,
                                    404,
                                ));
                                last_err = Some(anyhow::anyhow!(err));
                                if attempt < max_upload_attempts {
                                    let base = std::cmp::min(
                                        initial_retry_delay * 2u32.pow(attempt - 1),
                                        max_retry_delay,
                                    );
                                    tokio::time::sleep(jitter_duration(base)).await;
                                }
                                continue;
                            }
                            UploadAttemptOutcome::TransientRetry => {
                                // Transient transport / proxy issues during
                                // upload. Serde / Json here means GitHub
                                // returned a non-JSON body (typically an
                                // nginx/HAProxy 502/503 HTML page) while the
                                // error-mapping expected JSON: always
                                // transient, safe to retry. Route the
                                // per-attempt warn through the shared
                                // `format_retry_warn` helper so this bespoke
                                // loop cannot drift from the
                                // `retry_octocrab_call` helper's format.
                                let err = result.expect_err(
                                    "TransientRetry outcome guarantees Err variant",
                                );
                                let status = match &err {
                                    octocrab::Error::GitHub { source, .. } => {
                                        source.status_code.as_u16()
                                    }
                                    _ => 0,
                                };
                                let label = format!("upload of '{file_name}'");
                                release_log().warn(&format_retry_warn(
                                    &label,
                                    attempt,
                                    max_upload_attempts,
                                    status,
                                ));
                                last_err = Some(anyhow::anyhow!(err));
                                if attempt < max_upload_attempts {
                                    let base = std::cmp::min(
                                        initial_retry_delay * 2u32.pow(attempt - 1),
                                        max_retry_delay,
                                    );
                                    tokio::time::sleep(jitter_duration(base)).await;
                                }
                                continue;
                            }
                            UploadAttemptOutcome::Fatal => {
                                // Non-retryable error: fail immediately.
                                let err = result.expect_err(
                                    "Fatal outcome guarantees Err variant",
                                );
                                return Err(anyhow::anyhow!(err)).with_context(|| {
                                    format!(
                                        "release: upload artifact '{}' to release '{}'",
                                        file_name, tag_c
                                    )
                                });
                            }
                        }
                    }
                    if let Some(err) = last_err {
                        return Err(err).with_context(|| {
                            format!(
                                "release: upload artifact '{}' to release '{}' failed after {} attempts",
                                file_name, tag_c, max_upload_attempts
                            )
                        });
                    }

                    Ok::<String, anyhow::Error>(file_name)
                });
            }

            // Collect results from all upload tasks.
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(file_name)) => {
                        log.verbose(&format!("uploaded artifact: {}", file_name));
                    }
                    Ok(Err(e)) => return Err(e),
                    Err(join_err) => {
                        return Err(anyhow::anyhow!(
                            "release: upload task panicked: {}",
                            join_err
                        ));
                    }
                }
            }
        }

        // Draft-then-publish: if the user's config has draft=false,
        // un-draft the release now that all assets are uploaded.
        if !user_wants_draft {
            // Rate limit check before publish (un-draft) PATCH.
            check_github_rate_limit_with_env(&rate_limit_client, &token_str, 10, env_source).await;
            let publish_route = format!(
                "/repos/{}/{}/releases/{}",
                github.owner, github.name, release_id_raw
            );
            // Build the publish PATCH body via the GR-aligned helper
            // (GoReleaser PR #6591):
            // - includes `name` (re-rendered name_template) so the published
            //   release reflects the current template, even if the draft was
            //   created with an older name (commit
            //   `2e17678c4be30b1c53b5931919b57e71532b6d16`).
            // - forces `make_latest=false` whenever `prerelease` is true,
            //   regardless of the user's `make_latest` template (commit
            //   `6ecba31405e8ade89b335bf05e19734d0fd8d2d8`). A prerelease can
            //   never be the latest.
            let publish_body = build_publish_patch_body(
                release_name,
                prerelease,
                make_latest,
                discussion_category_name,
            );
            // Run the publish PATCH through the same `policy` used by every
            // other retriable octocrab call site. GitHub occasionally 502s
            // during un-draft when the release has many assets attached, and
            // the user-configurable `retry:` block is the surface that
            // controls how aggressively to retry. Defaults (10 attempts, 10s
            // base, 5m cap) match GoReleaser's `pkg/config.Retry` defaults.
            let _published: octocrab::models::repos::Release =
                retry_octocrab_call(&policy, "publish PATCH", Some(&retry_after_capture), || {
                    let publish_route = publish_route.clone();
                    let publish_body = publish_body.clone();
                    let octo = octo.clone();
                    async move {
                        octo.patch::<octocrab::models::repos::Release, _, _>(
                            publish_route,
                            Some(&publish_body),
                        )
                        .await
                    }
                })
                .await
                .with_context(|| {
                    format!(
                        "release: publish (un-draft) release '{}' on {}/{}",
                        tag, github.owner, github.name
                    )
                })?;
            if !retouch_live {
                log.status(&format!(
                    "published release '{}' (draft -> live)",
                    release_name
                ));
            }
        }

        Ok::<String, anyhow::Error>(html_url)
    })?;

    Ok(Some((
        url,
        gh_download_base,
        github.owner.clone(),
        github.name.clone(),
    )))
}

#[cfg(test)]
mod find_draft_by_name_tests {
    //! Behavioural pins for [`find_draft_by_name`] — the paginated draft
    //! search used by the `replace_existing_draft` and
    //! `use_existing_draft` paths in `run_github_backend`.
    //!
    //! These tests drive a real `octocrab::Octocrab` against an
    //! in-process loopback responder (the shared
    //! `spawn_oneshot_http_responder`) so the pagination terminator,
    //! per-page route shape, and `draft && name match` predicate are
    //! pinned against the production code path — not the matcher in
    //! isolation.
    use super::*;
    use crate::test_support::{build_test_octocrab, test_retry_policy};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;

    /// Build a minimal release JSON list of `count` entries, marking the
    /// one at `match_idx` (when `Some`) as a draft with `name=target`.
    /// Every other entry is published (`draft: false`) with a distinct
    /// name so the predicate's "draft && name match" requirement is
    /// exercised.
    fn build_release_list_body(
        count: usize,
        match_idx: Option<usize>,
        target_name: &str,
    ) -> String {
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let (draft, name) = match match_idx {
                Some(idx) if idx == i => (true, target_name.to_string()),
                _ => (false, format!("other-release-{i}")),
            };
            entries.push(serde_json::json!({
                "id": 1000 + i as u64,
                "node_id": format!("RL_{i}"),
                "tag_name": format!("v0.0.{i}"),
                "target_commitish": "main",
                "name": name,
                "draft": draft,
                "prerelease": false,
                "created_at": "2026-01-01T00:00:00Z",
                "published_at": null,
                "author": null,
                "assets": [],
                "tarball_url": null,
                "zipball_url": null,
                "body": null,
                "url": format!("https://api.github.com/repos/o/r/releases/{}", 1000 + i),
                "html_url": format!("https://github.com/o/r/releases/{}", 1000 + i),
                "assets_url": format!("https://api.github.com/repos/o/r/releases/{}/assets", 1000 + i),
                "upload_url": format!("https://uploads.github.com/repos/o/r/releases/{}/assets{{?name,label}}", 1000 + i),
            }));
        }
        serde_json::Value::Array(entries).to_string()
    }

    /// Build a static HTTP response carrying a JSON release-list body.
    fn build_release_list_response(body: String) -> &'static str {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        Box::leak(raw.into_boxed_str())
    }

    #[tokio::test]
    async fn single_page_with_matching_draft_returns_some() {
        let body = build_release_list_body(3, Some(1), "v1.2.3");
        let (addr, calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("draft search must succeed");
        let release = found.expect("draft with matching name must be found");
        assert_eq!(release.name.as_deref(), Some("v1.2.3"));
        assert!(release.draft, "matched release must be a draft");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "single-page search must issue exactly one list-releases call",
        );
    }

    #[tokio::test]
    async fn single_page_no_match_returns_none() {
        // Three published releases, none match the target name; the
        // predicate must not coerce a non-draft into a match.
        let body = build_release_list_body(3, None, "v9.9.9");
        let (addr, _calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v9.9.9", &policy, None)
            .await
            .expect("draft search must succeed");
        assert!(
            found.is_none(),
            "no draft matches the target name => Ok(None)",
        );
    }

    #[tokio::test]
    async fn name_matches_but_not_draft_returns_none() {
        // A *published* release whose name equals the target must NOT
        // match — the predicate requires `draft && name match`.
        let body = build_release_list_body(2, None, "ignored");
        // Patch entry 0 to have the target name but stay non-draft.
        let mut entries: Vec<serde_json::Value> = serde_json::from_str(&body).expect("array");
        entries[0]["name"] = serde_json::Value::String("v1.2.3".to_string());
        entries[0]["draft"] = serde_json::Value::Bool(false);
        let body = serde_json::Value::Array(entries).to_string();
        let (addr, _calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("draft search must succeed");
        assert!(
            found.is_none(),
            "published release with matching name must NOT count as a draft hit",
        );
    }

    #[tokio::test]
    async fn paginates_across_pages_until_match_found() {
        // Page 1: 100 non-matching published releases (forces another page).
        // Page 2: a draft with the matching name in slot 0.
        let page_1 = build_release_list_body(100, None, "v1.2.3");
        let page_2 = build_release_list_body(5, Some(0), "v1.2.3");
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            build_release_list_response(page_1),
            build_release_list_response(page_2),
        ]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("paginated draft search must succeed");
        let release = found.expect("draft on page 2 must be found");
        assert_eq!(release.name.as_deref(), Some("v1.2.3"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "pagination must consume exactly 2 list-releases calls (full first page + second page)",
        );
    }

    #[tokio::test]
    async fn paginates_to_exhaustion_returns_none() {
        // Page 1: 100 non-matching entries (full page => continue).
        // Page 2: 50 non-matching entries (< page size => terminate).
        let page_1 = build_release_list_body(100, None, "missing");
        let page_2 = build_release_list_body(50, None, "missing");
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            build_release_list_response(page_1),
            build_release_list_response(page_2),
        ]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "missing", &policy, None)
            .await
            .expect("draft search must succeed even when no match");
        assert!(
            found.is_none(),
            "exhausted listing with no match => Ok(None)",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "must fetch both pages before terminating on the partial page",
        );
    }
}

#[cfg(test)]
mod already_exists_tests {
    use super::*;

    #[test]
    fn idempotent_when_remote_matches_local_regardless_of_flag() {
        // Even with `replace_existing_artifacts: false`, a byte-identical
        // remote asset is a no-op: the user's guard rail is "don't
        // overwrite different bytes", not "don't probe the API".
        assert_eq!(
            classify_already_exists(false, Some(100), 100),
            AlreadyExistsAction::SkipIdempotent,
        );
        assert_eq!(
            classify_already_exists(true, Some(100), 100),
            AlreadyExistsAction::SkipIdempotent,
        );
    }

    #[test]
    fn bails_when_replace_forbidden_and_sizes_differ() {
        // GR parity: `if !ReplaceExistingArtifacts { return Unrecoverable }`.
        // Surfaces the conflict instead of silently overwriting.
        assert_eq!(
            classify_already_exists(false, Some(100), 200),
            AlreadyExistsAction::BailReplaceForbidden,
        );
        // `remote_size: None` (asset present but size unknown) is treated
        // as a size-mismatch: better to bail than silently overwrite.
        assert_eq!(
            classify_already_exists(false, None, 200),
            AlreadyExistsAction::BailReplaceForbidden,
        );
    }

    #[test]
    fn deletes_and_retries_when_replace_allowed_and_sizes_differ() {
        assert_eq!(
            classify_already_exists(true, Some(100), 200),
            AlreadyExistsAction::DeleteAndRetry,
        );
        assert_eq!(
            classify_already_exists(true, None, 200),
            AlreadyExistsAction::DeleteAndRetry,
        );
    }
}

#[cfg(test)]
mod get_by_tag_lookup_tests {
    //! Pin the `get_by_tag` lookup decision rule introduced to prevent the
    //! "transient 5xx falls through to create-release POST" bug.
    //!
    //! Two invariants:
    //! 1. The lookup is retried per the user's `RetryPolicy` (transient 5xx /
    //!    429 / transport failures retry). The retry-loop contract itself is
    //!    pinned by `retry_call::tests` against a real TCP responder.
    //! 2. Only a real 404 yields "no existing release" (None); every other
    //!    error (auth, validation, exhausted retries on 5xx) propagates so
    //!    the user sees the real GitHub error, NOT a downstream 422
    //!    "tag already exists" from the create-release POST.
    //!
    //! The tests below focus on the routing predicate `is_octocrab_404`
    //! against real `octocrab::Error::GitHub` values. The retry-then-error
    //! coupling is exercised by `retry_call::tests` plus a single 404
    //! fast-fail check here so the predicate's "404 only" invariant is
    //! pinned end-to-end against the helper.
    use super::*;
    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[tokio::test]
    async fn is_octocrab_404_matches_only_404_github_variant() {
        // The pure predicate's contract: returns true for
        // `Error::GitHub { source }` with status_code 404, false for every
        // other variant or status.
        let github_err_404 = synth_github_error(404).await;
        assert!(
            is_octocrab_404(&github_err_404),
            "404 status_code on GitHub variant must classify as 404"
        );
        let github_err_503 = synth_github_error(503).await;
        assert!(
            !is_octocrab_404(&github_err_503),
            "503 must NOT classify as 404 (would let the caller fall \
             through to create-release and surface a downstream 422)"
        );
        let github_err_422 = synth_github_error(422).await;
        assert!(
            !is_octocrab_404(&github_err_422),
            "422 must NOT classify as 404"
        );
        let github_err_500 = synth_github_error(500).await;
        assert!(
            !is_octocrab_404(&github_err_500),
            "500 must NOT classify as 404"
        );
    }

    #[tokio::test]
    async fn get_by_tag_404_fast_fails_through_helper_to_predicate() {
        // End-to-end: drive a 404 through `retry_octocrab_call` and confirm
        // the returned typed error satisfies `is_octocrab_404`, so the
        // backend's match arm maps the lookup to "no existing release"
        // (the only non-error fall-through to create-release).
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"message\":\"Not Found\"}",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(result.is_err(), "404 must surface as Err from the helper");
        let err = result.expect_err("err is Some by the assert above");
        assert!(
            is_octocrab_404(&err),
            "404 must classify so the caller maps to None: got {err:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "404 must NOT retry (fast-fail honors classifier)"
        );
    }

    #[tokio::test]
    async fn get_by_tag_5xx_retries_then_succeeds_under_helper() {
        // Pin the regression: a transient 5xx on `get_by_tag` must retry
        // through `retry_octocrab_call`, NOT fall through to the
        // create-release POST (which would surface a 422 "tag already
        // exists" on a tag whose existing release just had a flaky lookup).
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<serde_json::Value, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(
            result.is_ok(),
            "5xx must retry to success under the get_by_tag label: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 2 retries past 5xx + 1 success"
        );
    }

    #[tokio::test]
    async fn get_by_tag_500_forever_surfaces_real_error_not_404_fallthrough() {
        // Pin the regression: if every retry sees 5xx, the helper must
        // surface the typed 500 error (NOT swallow it into None). The
        // backend's match arm has only one non-error fall-through (a real
        // 404 via `is_octocrab_404`); 500-forever must propagate so the
        // user sees the real GitHub error instead of a confusing downstream
        // 422 "tag already exists" from create-release.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<serde_json::Value, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(
            result.is_err(),
            "500-forever must surface as Err, NOT swallow into None"
        );
        let err = result.expect_err("err is Some by the assert above");
        assert!(
            !is_octocrab_404(&err),
            "500-forever must NOT classify as 404; the backend's only \
             non-error fall-through is 404, so misclassifying here would \
             trigger the original bug: get_by_tag 5xx -> create-release \
             POST -> 422. Got: {err:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "max_attempts=3 must produce exactly 3 octocrab calls"
        );
    }

    /// Synthesize an `octocrab::Error::GitHub` with a chosen status code by
    /// round-tripping a minimal GitHub error body through the live API
    /// envelope. octocrab's `*Snafu` builders are private, so we cannot
    /// construct the variant directly; the canonical path is to drive an
    /// HTTP response through octocrab and capture the resulting `Err`.
    async fn synth_github_error(status: u16) -> octocrab::Error {
        let body = serde_json::json!({
            "message": "synthetic",
            "documentation_url": "https://example/synthetic"
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 {status} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        let static_resp: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![static_resp]);
        let octo = build_test_octocrab(addr);
        octo.get::<serde_json::Value, _, _>("/synthetic", None::<&()>)
            .await
            .expect_err("synth_github_error: octocrab must surface Err for non-2xx status")
    }

    fn build_test_octocrab(addr: SocketAddr) -> octocrab::Octocrab {
        let builder = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://{addr}/"))
            .expect("OctocrabBuilder::base_uri accepts loopback URL");
        builder
            .build()
            .expect("OctocrabBuilder::build succeeds on loopback URL")
    }
}

#[cfg(test)]
mod existing_assets_precheck_tests {
    use super::*;

    // Argument order across the helper:
    //   (skip_upload, resume_release, replace_existing_artifacts, asset_names)

    #[test]
    fn no_conflict_when_release_has_no_assets() {
        let result = check_existing_assets_block_upload(false, false, false, &[]);
        assert!(result.is_none(), "empty asset list must not block");
    }

    #[test]
    fn no_conflict_when_replace_existing_is_true() {
        let result = check_existing_assets_block_upload(false, false, true, &["foo.tar.gz"]);
        assert!(
            result.is_none(),
            "replace_existing_artifacts=true permits overwrite"
        );
    }

    #[test]
    fn no_conflict_when_skip_upload_is_true() {
        let result = check_existing_assets_block_upload(true, false, false, &["foo.tar.gz"]);
        assert!(result.is_none(), "skip_upload=true means nothing to upload");
    }

    #[test]
    fn no_conflict_when_resume_release_is_true() {
        // `--resume-release` is the user's explicit opt-in to continue into
        // an existing release: the pre-check must NOT bail even when assets
        // are present and replace_existing_artifacts is false.
        let result =
            check_existing_assets_block_upload(false, true, false, &["foo.tar.gz", "bar.zip"]);
        assert!(
            result.is_none(),
            "--resume-release must bypass the pre-check"
        );
    }

    #[test]
    fn no_conflict_when_replace_existing_cli_override_is_true() {
        // The CLI override is plumbed via `replace_existing_artifacts: true`
        // in the helper signature (the caller ORs the config value with
        // ctx.options.replace_existing_artifacts before calling).
        // This pins that the helper treats the CLI-derived value the same
        // as the config-derived value.
        let result =
            check_existing_assets_block_upload(false, false, true, &["foo.tar.gz", "bar.zip"]);
        assert!(
            result.is_none(),
            "--replace-existing must bypass the pre-check via replace_existing_artifacts=true"
        );
    }

    #[test]
    fn conflicts_when_assets_present_and_replace_forbidden() {
        // The scenario that was previously unrecoverable: partial assets
        // from a prior failed attempt exist, and replace_existing_artifacts
        // is false. The helper must surface them so the caller can bail.
        let assets = &["app_linux_amd64.tar.gz", "checksums.txt"];
        let result = check_existing_assets_block_upload(false, false, false, assets);
        let names = result.expect("should detect conflict");
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"app_linux_amd64.tar.gz".to_string()));
        assert!(names.contains(&"checksums.txt".to_string()));
    }

    #[test]
    fn conflict_list_preserves_input_order() {
        // The helper returns the names in the order the caller supplied
        // them, so the resulting bail message lists assets in a predictable
        // (release-API) order. A future sort/dedupe regression would be
        // user-visible noise; pin the contract.
        let assets = &["a.tar.gz", "b.zip", "c.sig"];
        let names = check_existing_assets_block_upload(false, false, false, assets)
            .expect("conflict present");
        assert_eq!(
            names,
            vec![
                "a.tar.gz".to_string(),
                "b.zip".to_string(),
                "c.sig".to_string()
            ]
        );
    }

    #[test]
    fn skip_upload_wins_even_with_assets_and_no_replace() {
        // skip_upload short-circuits BEFORE the asset-list inspection runs.
        // Pinning this so a future refactor doesn't reorder the early-return
        // and accidentally surface a conflict during a no-op upload pass.
        let result = check_existing_assets_block_upload(true, false, false, &["x.tar.gz"]);
        assert!(
            result.is_none(),
            "skip_upload short-circuits unconditionally"
        );
    }
}

#[cfg(test)]
mod upload_retry_locals_tests {
    //! Pin the policy-to-locals translation that the bespoke upload retry
    //! loop reads on every iteration. The formula is trivial today but the
    //! rustdoc claims "single point of translation"; if a future change
    //! adds a clamp / fudge factor / multiplier here, these tests force
    //! that change to be conscious (and visible in one place).
    use super::*;
    use anodizer_core::retry::RetryPolicy;
    use std::time::Duration;

    #[test]
    fn returns_policy_fields_verbatim() {
        let policy = RetryPolicy {
            max_attempts: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(30),
        };
        let (attempts, base, max) = upload_retry_locals(&policy);
        assert_eq!(
            attempts, 7,
            "max_attempts mirrors RetryPolicy::max_attempts"
        );
        assert_eq!(base, Duration::from_millis(50));
        assert_eq!(max, Duration::from_secs(30));
    }

    #[test]
    fn surfaces_the_upload_canonical_policy_unchanged() {
        // GoReleaser-aligned canonical upload policy: 10 attempts, 50ms base,
        // 30s cap. The locals helper must NOT mutate these on the way to the
        // upload loop — drift here is a user-visible behaviour change in the
        // retry envelope.
        let (attempts, base, max) = upload_retry_locals(&RetryPolicy::UPLOAD);
        assert_eq!(attempts, 10);
        assert_eq!(base, Duration::from_millis(50));
        assert_eq!(max, Duration::from_secs(30));
    }

    #[test]
    fn preserves_one_attempt_minimum_without_extra_clamp() {
        // The rustdoc claims the helper relies on RetryConfig::to_policy's
        // upstream clamp and adds none of its own. A `max_attempts: 1`
        // input must therefore round-trip unchanged (proving the helper
        // does not, say, force a minimum of 2 retries).
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let (attempts, _, _) = upload_retry_locals(&policy);
        assert_eq!(
            attempts, 1,
            "single-attempt policy must round-trip verbatim"
        );
    }
}

#[cfg(test)]
mod already_exists_action_derive_tests {
    //! Pin the `Debug`/`PartialEq`/`Eq` derives on `AlreadyExistsAction`.
    //! The classifier returns these variants and downstream call sites in
    //! the upload retry loop `match` on them — a drift to a non-equality
    //! representation would silently break the upload loop's arm matching.
    use super::*;

    #[test]
    fn variants_compare_equal_only_to_themselves() {
        assert_eq!(
            AlreadyExistsAction::SkipIdempotent,
            AlreadyExistsAction::SkipIdempotent
        );
        assert_ne!(
            AlreadyExistsAction::SkipIdempotent,
            AlreadyExistsAction::BailReplaceForbidden
        );
        assert_ne!(
            AlreadyExistsAction::BailReplaceForbidden,
            AlreadyExistsAction::DeleteAndRetry
        );
        assert_ne!(
            AlreadyExistsAction::DeleteAndRetry,
            AlreadyExistsAction::SkipIdempotent
        );
    }

    #[test]
    fn debug_format_names_the_variant() {
        // The error-path log lines format the action via `{:?}` to identify
        // which branch the classifier picked. Pin the variant names so a
        // future rename (`SkipIdempotent` -> `Idempotent`) surfaces in the
        // log diff instead of silently breaking grep-based triage.
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::SkipIdempotent),
            "SkipIdempotent"
        );
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::BailReplaceForbidden),
            "BailReplaceForbidden"
        );
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::DeleteAndRetry),
            "DeleteAndRetry"
        );
    }
}

#[cfg(test)]
mod spec_struct_surface_tests {
    //! Pin the field surface of the three "context bundles" passed
    //! into `run_github_backend`. Each is `Clone + Copy` so a struct
    //! can be constructed, copied, and read field-by-field through
    //! the copy — a future field removal/rename breaks compilation
    //! here, not at the distant call site in `run.rs`.
    use super::*;
    use octocrab::repos::releases::MakeLatest;

    #[test]
    fn github_release_spec_round_trips_all_fields() {
        let make_latest = Some(MakeLatest::True);
        let target = Some("main".to_string());
        let category = Some("Announcements".to_string());
        let spec = GithubReleaseSpec {
            tag: "v1.2.3",
            name: "Release 1.2.3",
            body: "## Changes",
            mode: "replace",
            draft: true,
            prerelease: false,
            make_latest: &make_latest,
            target_commitish: &target,
            discussion_category: &category,
        };
        let copy = spec; // exercises Copy
        assert_eq!(copy.tag, "v1.2.3");
        assert_eq!(copy.name, "Release 1.2.3");
        assert_eq!(copy.body, "## Changes");
        assert_eq!(copy.mode, "replace");
        assert!(copy.draft);
        assert!(!copy.prerelease);
        assert!(copy.make_latest.is_some());
        assert_eq!(copy.target_commitish.as_deref(), Some("main"));
        assert_eq!(copy.discussion_category.as_deref(), Some("Announcements"));
    }

    #[test]
    fn upload_opts_round_trips_every_field() {
        // Independent fields -> a drift in field order or a silent removal
        // would let the caller in `run.rs` send `replace_existing_draft`
        // where `skip_upload` was wanted. Pin each one by name.
        let opts = UploadOpts {
            skip_upload: true,
            replace_existing_draft: false,
            replace_existing_artifacts: true,
            use_existing_draft: false,
            resume_release: true,
            retention_keep_last: Some(10),
            publish_repo_override: Some(("nushell".to_string(), "nightly".to_string())),
        };
        let copy = opts.clone();
        assert!(copy.skip_upload);
        assert!(!copy.replace_existing_draft);
        assert!(copy.replace_existing_artifacts);
        assert!(!copy.use_existing_draft);
        assert!(copy.resume_release);
        assert_eq!(copy.retention_keep_last, Some(10));
        assert_eq!(
            copy.publish_repo_override,
            Some(("nushell".to_string(), "nightly".to_string()))
        );
    }

    #[test]
    fn upload_opts_all_false_is_constructible() {
        // The "default-ish" shape (no opt-ins): the upload loop must see
        // every flag as `false` so the production code path runs as the
        // GR-aligned default. A drift to e.g. `Option<bool>` would break
        // this all-false literal.
        let opts = UploadOpts {
            skip_upload: false,
            replace_existing_draft: false,
            replace_existing_artifacts: false,
            use_existing_draft: false,
            resume_release: false,
            retention_keep_last: None,
            publish_repo_override: None,
        };
        assert!(!opts.skip_upload);
        assert!(!opts.replace_existing_draft);
        assert!(!opts.replace_existing_artifacts);
        assert!(!opts.use_existing_draft);
        assert!(!opts.resume_release);
        assert_eq!(opts.retention_keep_last, None);
        assert_eq!(opts.publish_repo_override, None);
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_one_prunes_all() {
        // keep_last=1 (the keep_single_release alias): every existing nightly
        // release is pruned — only the about-to-be-created one survives.
        let existing = vec![
            (3u64, "0.1.0-nightly.2".to_string()),
            (2u64, "0.1.0-nightly.1".to_string()),
            (1u64, "0.1.0-nightly.0".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&existing, 1);
        assert_eq!(pruned, existing);
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_n_keeps_newest() {
        // keep_last=2: with the new release counting as the newest, retain
        // only the single newest existing release; prune the older two.
        let existing = vec![
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
            (1u64, "t1".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&existing, 2);
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())]
        );
    }

    #[test]
    fn nightly_releases_to_prune_keeps_all_when_under_budget() {
        // Fewer existing releases than (keep_last - 1): nothing to prune.
        let existing = vec![(1u64, "t1".to_string())];
        assert!(nightly_releases_to_prune(&existing, 10).is_empty());
    }

    #[test]
    fn nightly_releases_to_prune_floors_zero_to_one() {
        let existing = vec![(1u64, "t1".to_string())];
        // keep_last=0 floored to 1 -> prune everything.
        assert_eq!(nightly_releases_to_prune(&existing, 0), existing);
    }

    #[test]
    fn nightly_releases_to_prune_sorts_out_of_order_input() {
        // API response order must not matter: feed ids out of order and
        // assert the newest (highest id) is the one kept.
        let existing = vec![
            (1u64, "t1".to_string()),
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
        ];
        // keep_last=2: keep the single newest existing (id=3), prune 2 and 1
        // in newest-first order.
        let pruned = nightly_releases_to_prune(&existing, 2);
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())],
            "must keep the highest-id release regardless of input order",
        );
    }
}

#[cfg(test)]
mod orchestrator_tests {
    //! End-to-end coverage for [`run_github_backend`] dispatch paths.
    //!
    //! These tests drive the orchestrator against a scripted in-process
    //! HTTP responder so the create-vs-update-vs-replace branching,
    //! upload-asset happy path, and 422 `already_exists` recovery arms
    //! are pinned against the production wiring — not just the helper
    //! classifiers (which have their own unit tests).
    //!
    //! ## Fixture wiring
    //!
    //! Every test points two URL surfaces at the loopback responder:
    //!
    //! - `ctx.config.github_urls.api` / `.upload` — the octocrab
    //!   builder honors these, so every API call (list / create /
    //!   PATCH / asset list / asset delete) routes through
    //!   `http://addr/`. The release JSON returned by POST /releases
    //!   carries `upload_url: http://addr/...` so `upload_asset(...)`
    //!   POSTs to the same loopback.
    //! - `ANODIZER_GITHUB_API_BASE` — the rate-limit poll honors this
    //!   override. `build_ctx` seeds it through the [`Context`]'s
    //!   injected [`MapEnvSource`](anodizer_core::MapEnvSource) so
    //!   the proactive `/rate_limit` poll either matches a scripted
    //!   route or silently degrades on 404, never delaying the test.
    //!
    //! Env injection is per-[`Context`], so parallel tests cannot race
    //! and no global env-mutex is required.

    use super::*;
    use anodizer_core::config::{CrateConfig, GitHubUrlsConfig, ReleaseConfig, ScmRepoConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder_on,
    };
    use octocrab::repos::releases::MakeLatest;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Wrap a JSON body in a `200 OK` HTTP response with the correct
    /// `Content-Length`. Leaks the formatted string because the responder
    /// requires `&'static str`; harmless in tests.
    fn http_ok(body: String) -> &'static str {
        let len = body.len();
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
    }

    /// Same as [`http_ok`] but emits `201 Created`. GitHub returns 201 for
    /// release create + asset upload; the orchestrator does not distinguish
    /// 200 vs 201, but using the realistic status keeps the fixture honest.
    fn http_201(body: String) -> &'static str {
        let len = body.len();
        Box::leak(
            format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
    }

    /// `204 No Content` for successful DELETE.
    const HTTP_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    /// Build a minimal Release JSON octocrab can deserialize into
    /// `models::repos::Release`. The `upload_url` field is the load-bearing
    /// one: `upload_asset(...).send()` does a GET on the release and reads
    /// `upload_url` to determine where to POST the asset bytes.
    fn release_json(addr: SocketAddr, id: u64, draft: bool, name: &str) -> String {
        serde_json::json!({
            "id": id,
            "node_id": format!("RL_{id}"),
            "tag_name": "v1.2.3",
            "target_commitish": "main",
            "name": name,
            "draft": draft,
            "prerelease": false,
            "created_at": "2026-01-01T00:00:00Z",
            "published_at": null,
            "author": null,
            "assets": [],
            "tarball_url": null,
            "zipball_url": null,
            "body": null,
            "url": format!("http://{addr}/repos/o/r/releases/{id}"),
            "html_url": format!("http://{addr}/o/r/releases/{id}"),
            "assets_url": format!("http://{addr}/repos/o/r/releases/{id}/assets"),
            // upload_url MUST carry the `{?name,label}` template that
            // octocrab strips before appending `?name=<file>`. Without the
            // template, octocrab leaves the URL malformed and the upload
            // POSTs to the wrong path.
            "upload_url": format!("http://{addr}/upload/{id}{{?name,label}}"),
        })
        .to_string()
    }

    /// Like [`release_json`] but with an explicit `tag_name` (distinct nightly
    /// tags such as `…-nightly.<build>` need their own tag for the retention
    /// sweep's tag-delete assertions). Targets owner=o/repo=r for the API URLs,
    /// matching the override-repo responder used by the retention tests.
    fn release_json_named(addr: SocketAddr, id: u64, name: &str, tag: &str) -> String {
        serde_json::json!({
            "id": id,
            "node_id": format!("RL_{id}"),
            "tag_name": tag,
            "target_commitish": "main",
            "name": name,
            "draft": false,
            "prerelease": false,
            "created_at": "2026-01-01T00:00:00Z",
            "published_at": null,
            "author": null,
            "assets": [],
            "tarball_url": null,
            "zipball_url": null,
            "body": null,
            "url": format!("http://{addr}/repos/o/r/releases/{id}"),
            "html_url": format!("http://{addr}/o/r/releases/{id}"),
            "assets_url": format!("http://{addr}/repos/o/r/releases/{id}/assets"),
            "upload_url": format!("http://{addr}/upload/{id}{{?name,label}}"),
        })
        .to_string()
    }

    /// Minimal Asset JSON for the 201 response of an asset-upload POST.
    fn asset_json(id: u64, name: &str, size: u64) -> String {
        serde_json::json!({
            "url": format!("http://example.test/asset/{id}"),
            "browser_download_url": format!("http://example.test/dl/{name}"),
            "id": id,
            "node_id": format!("RA_{id}"),
            "name": name,
            "label": null,
            "state": "uploaded",
            "content_type": "application/octet-stream",
            "size": size,
            "download_count": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "uploader": null,
        })
        .to_string()
    }

    /// 422 already_exists body. Pairs with HTTP status 422; the upload
    /// classifier matches `errors[].code == "already_exists"`.
    fn http_422_already_exists() -> &'static str {
        let body = r#"{"message":"Validation Failed","errors":[{"resource":"ReleaseAsset","code":"already_exists","field":"name"}]}"#;
        let len = body.len();
        Box::leak(
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}")
                .into_boxed_str(),
        )
    }

    /// Build a [`Context`] with `github_urls` pointing at `addr` so every
    /// production octocrab call routes through the loopback responder, and
    /// a fast retry policy (millisecond delays) so the upload retry loop
    /// in [`run_github_backend`] doesn't pad test runs with the production
    /// 10-second default backoff.
    fn build_ctx(addr: SocketAddr) -> Context {
        let base = format!("http://{addr}");
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.2.3")
            .token(Some("test-token".to_string()))
            .env("ANODIZER_GITHUB_API_BASE", &base)
            .build();
        ctx.config.github_urls = Some(GitHubUrlsConfig {
            api: Some(base.clone()),
            upload: Some(base.clone()),
            download: Some(base),
            skip_tls_verify: None,
        });
        ctx.config.retry = Some(anodizer_core::config::RetryConfig {
            attempts: 5,
            delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        });
        ctx
    }

    /// Build a `CrateConfig` whose `release.github` points at owner=o, name=r.
    fn build_crate_cfg() -> CrateConfig {
        let mut crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            ..Default::default()
        };
        crate_cfg.release = Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: "o".to_string(),
                name: "r".to_string(),
            }),
            mode: Some("replace".to_string()),
            ..Default::default()
        });
        crate_cfg
    }

    /// Write a small artifact file and return its path. The `run_github_backend`
    /// upload loop calls `std::fs::read` and uses the file's bytes (and
    /// length) for the upload POST + 422 size-compare branch.
    fn write_artifact(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write artifact");
        path
    }

    /// Owned ancillary fields that [`GithubReleaseSpec`] borrows. Bind in
    /// the test scope then pass into [`make_spec`] so the borrows outlive
    /// the spec struct.
    struct SpecAncillary {
        make_latest: Option<MakeLatest>,
        target_commitish: Option<String>,
        discussion_category: Option<String>,
    }

    fn spec_ancillary_default() -> SpecAncillary {
        SpecAncillary {
            make_latest: None,
            target_commitish: None,
            discussion_category: None,
        }
    }

    /// Common spec: tag=v1.2.3, draft=true (so `user_wants_draft` short-circuits
    /// the publish PATCH), mode=replace (so `get_by_tag` lookup is skipped).
    fn make_spec(anc: &SpecAncillary) -> GithubReleaseSpec<'_> {
        GithubReleaseSpec {
            tag: "v1.2.3",
            name: "v1.2.3",
            body: "release body",
            mode: "replace",
            draft: true,
            prerelease: false,
            make_latest: &anc.make_latest,
            target_commitish: &anc.target_commitish,
            discussion_category: &anc.discussion_category,
        }
    }

    /// Default upload opts: every flag off.
    fn base_opts() -> UploadOpts {
        UploadOpts {
            skip_upload: false,
            replace_existing_draft: false,
            replace_existing_artifacts: false,
            use_existing_draft: false,
            resume_release: false,
            retention_keep_last: None,
            publish_repo_override: None,
        }
    }

    /// `run_github_backend`'s success payload: `(html_url, download_base,
    /// owner, repo)` or `None` when the backend signals skip.
    type BackendOutcome = Result<Option<(String, String, String, String)>>;

    /// Build the four ambient handles `run_github_backend` consumes.
    fn run_backend(
        rt: &tokio::runtime::Runtime,
        ctx: &Context,
        token: &Option<String>,
        crate_cfg: &CrateConfig,
        spec: &GithubReleaseSpec<'_>,
        opts: &UploadOpts,
        artifacts: &[(PathBuf, Option<String>)],
    ) -> BackendOutcome {
        let log = StageLogger::new("release", Verbosity::Normal);
        let env = BackendEnv {
            rt,
            ctx,
            log: &log,
            token,
        };
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg present");
        run_github_backend(&env, crate_cfg, release_cfg, spec, opts, artifacts)
    }

    /// Like [`run_backend`] but attaches a [`LogCapture`] so a test can assert
    /// on the status lines the backend emits (not just the HTTP calls it makes).
    #[allow(clippy::too_many_arguments)]
    fn run_backend_capturing(
        rt: &tokio::runtime::Runtime,
        ctx: &Context,
        token: &Option<String>,
        crate_cfg: &CrateConfig,
        spec: &GithubReleaseSpec<'_>,
        opts: &UploadOpts,
        artifacts: &[(PathBuf, Option<String>)],
    ) -> (BackendOutcome, anodizer_core::log::LogCapture) {
        let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
        let env = BackendEnv {
            rt,
            ctx,
            log: &log,
            token,
        };
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg present");
        let result = run_github_backend(&env, crate_cfg, release_cfg, spec, opts, artifacts);
        (result, capture)
    }

    // ---------------------------------------------------------------------
    // 1. Happy path — create new release, upload one asset.
    // ---------------------------------------------------------------------
    #[test]
    fn create_release_and_upload_one_asset_succeeds() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        // Reserve an ephemeral port then drop the listener so the scripted
        // responder can claim the same port — the release_json fixture
        // needs to embed the bound addr into `upload_url`, which the
        // upload_asset() flow reads back to route its POST.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        let routes = vec![
            // (1) Create-release POST.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            // (2) upload_asset() first GETs the release to read upload_url.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            // (3) The asset POST itself.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        let result = run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("run_github_backend succeeds");
        let (html_url, dl_base, owner, repo) = result.expect("returns Some on success");
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        // gh_download_base derives from github_urls.download (set to
        // the loopback by build_ctx); html_url composes deterministically
        // from it.
        assert!(
            html_url.contains("/o/r/releases/tag/v1.2.3"),
            "got: {html_url}"
        );
        assert!(dl_base.starts_with("http://"), "got: {dl_base}");

        let entries = log.lock().expect("log mutex");
        let post_create = entries
            .iter()
            .find(|e| e.method == "POST" && e.path == "/repos/o/r/releases")
            .expect("must POST /repos/o/r/releases to create the release");
        assert!(
            post_create.body.contains("\"name\":\"v1.2.3\""),
            "create body must include the release name: {}",
            post_create.body
        );
        assert!(
            post_create.body.contains("\"draft\":true"),
            "create body must request draft=true (draft-then-publish): {}",
            post_create.body
        );
        let upload_call = entries
            .iter()
            .find(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
            .expect("must POST the asset to the upload_url returned in the release JSON");
        assert_eq!(
            upload_call.body, "hello world",
            "upload body must equal the file bytes"
        );
    }

    // ---------------------------------------------------------------------
    // 2. replace_existing_draft = true — find existing draft, delete it,
    // then create a new release.
    // ---------------------------------------------------------------------
    #[test]
    fn replace_existing_draft_deletes_then_creates() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"payload");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Existing draft (id=99) returned by list-releases.
        let list_body = format!("[{}]", release_json(addr, 99, true, "v1.2.3"));
        // New draft (id=42) created after the delete.
        let new_release = release_json(addr, 42, true, "v1.2.3");

        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases?per_page=100&page=1",
                response: http_ok(list_body),
                times: Some(1),
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/repos/o/r/releases/99",
                response: HTTP_204,
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(new_release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(new_release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

        let mut opts = base_opts();
        opts.replace_existing_draft = true;
        let anc = spec_ancillary_default();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &opts,
            &artifacts,
        )
        .expect("backend succeeds")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/99"),
            "must DELETE the existing draft (id=99); calls: {entries:?}",
        );
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
            "must POST a fresh release after the delete; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // nightly publish_repo: the release create, asset upload, AND the
    // composed html_url all target the OVERRIDE repo (nushell/nightly),
    // not the source repo (o/r) resolved from release.github.
    // ---------------------------------------------------------------------
    #[test]
    fn publish_repo_override_redirects_create_and_upload() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Override repo's API URLs use /repos/nushell/nightly/...
        let release = serde_json::json!({
            "id": 42, "node_id": "RL_42", "tag_name": "v1.2.3",
            "target_commitish": "main", "name": "v1.2.3", "draft": true,
            "prerelease": false, "created_at": "2026-01-01T00:00:00Z",
            "published_at": null, "author": null, "assets": [],
            "tarball_url": null, "zipball_url": null, "body": null,
            "url": format!("http://{addr}/repos/nushell/nightly/releases/42"),
            "html_url": format!("http://{addr}/nushell/nightly/releases/42"),
            "assets_url": format!("http://{addr}/repos/nushell/nightly/releases/42/assets"),
            "upload_url": format!("http://{addr}/upload/42{{?name,label}}"),
        })
        .to_string();

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/nushell/nightly/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/nushell/nightly/releases/42",
                response: http_ok(release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_a, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        let mut opts = base_opts();
        opts.publish_repo_override = Some(("nushell".to_string(), "nightly".to_string()));

        let result = run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &opts,
            &artifacts,
        )
        .expect("backend succeeds");
        let (html_url, _dl, owner, repo) = result.expect("returns Some");
        // Returned owner/repo + html_url reflect the OVERRIDE repo.
        assert_eq!(owner, "nushell");
        assert_eq!(repo, "nightly");
        assert!(
            html_url.contains("/nushell/nightly/releases/tag/v1.2.3"),
            "got: {html_url}"
        );

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/repos/nushell/nightly/releases"),
            "create must target the override repo; calls: {entries:?}",
        );
        // No call may touch the source repo (o/r).
        assert!(
            !entries.iter().any(|e| e.path.starts_with("/repos/o/r/")),
            "no call may target the source repo o/r; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // nightly retention keep_last=2: list nightly releases by name, keep the
    // newest 1 existing (the new one becomes the 2nd), DELETE the older
    // release AND its distinct git tag.
    // ---------------------------------------------------------------------
    #[test]
    fn retention_keep_last_prunes_old_release_and_tag() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"x");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Two existing nightly releases sharing the name "demo-nightly" but
        // with distinct tags (newest-first, as GitHub lists them). The new
        // release (tag v1.2.3) will become the newest, so with keep_last=2 we
        // keep id=11 and prune id=10 + its tag "nightly.0".
        let list_body = format!(
            "[{},{}]",
            release_json_named(addr, 11, "demo-nightly", "nightly.1"),
            release_json_named(addr, 10, "demo-nightly", "nightly.0"),
        );
        let new_release = release_json_named(addr, 42, "demo-nightly", "v1.2.3");

        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases?per_page=100&page=1",
                response: http_ok(list_body),
                times: Some(1),
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/repos/o/r/releases/10",
                response: HTTP_204,
                times: Some(1),
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/repos/o/r/git/refs/tags/nightly.0",
                response: HTTP_204,
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(new_release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(new_release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_a, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();
        // The nightly release name the sweep matches on.
        let spec = GithubReleaseSpec {
            name: "demo-nightly",
            ..make_spec(&anc)
        };

        let mut opts = base_opts();
        opts.retention_keep_last = Some(2);

        run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
            .expect("backend succeeds")
            .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/10"),
            "must delete the pruned release id=10; calls: {entries:?}",
        );
        assert!(
            entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/git/refs/tags/nightly.0"),
            "must delete the pruned release's distinct git tag; calls: {entries:?}",
        );
        // The kept release (id=11) must NOT be deleted.
        assert!(
            !entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/11"),
            "must KEEP the newest existing release id=11; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // replace_existing_draft = true with the NEW release published
    // (`draft: false`): the leftover draft must still be deleted. This pins
    // the self-heal path: publishes while replacing a stale
    // draft from a prior failed run; gating the delete on the new release's
    // draft flag would skip cleanup and the stale id later 404s on upload.
    // ---------------------------------------------------------------------
    #[test]
    fn replace_existing_draft_deletes_when_publishing() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"payload");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Existing draft (id=99) returned by list-releases.
        let list_body = format!("[{}]", release_json(addr, 99, true, "v1.2.3"));
        // New PUBLISHED release (id=42, draft=false) created after the delete.
        let new_release = release_json(addr, 42, false, "v1.2.3");

        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases?per_page=100&page=1",
                response: http_ok(list_body),
                times: Some(1),
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/repos/o/r/releases/99",
                response: HTTP_204,
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(new_release.clone()),
                times: Some(1),
            },
            // Un-draft PATCH: the release is created as a draft then flipped
            // live because the spec requests `draft: false`.
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(new_release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(new_release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

        let mut opts = base_opts();
        opts.replace_existing_draft = true;
        let anc = spec_ancillary_default();
        // Publish (draft: false) while replacing a stale draft — the self-heal recovery path.
        let mut spec = make_spec(&anc);
        spec.draft = false;
        run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
            .expect("backend succeeds")
            .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/99"),
            "must DELETE the stale draft (id=99) even when publishing; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 3. use_existing_draft = true — find existing draft, PATCH it (no POST).
    // ---------------------------------------------------------------------
    #[test]
    fn use_existing_draft_patches_instead_of_posting() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"data");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let existing = release_json(addr, 55, true, "v1.2.3");
        let list_body = format!("[{}]", existing.clone());

        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases?per_page=100&page=1",
                response: http_ok(list_body),
                times: Some(1),
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/o/r/releases/55",
                response: http_ok(existing.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/55",
                response: http_ok(existing),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/55?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

        let mut opts = base_opts();
        opts.use_existing_draft = true;
        let anc = spec_ancillary_default();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &opts,
            &artifacts,
        )
        .expect("backend succeeds")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "PATCH" && e.path == "/repos/o/r/releases/55"),
            "use_existing_draft must PATCH the existing release; calls: {entries:?}",
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
            "use_existing_draft must NOT POST a new release (would 422 on duplicate tag); calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 3b. keep-existing re-touch of an already-live release — the publish
    //     pipeline pass that runs after the release stage already created and
    //     published the release. The PATCH stays idempotent, but the
    //     create/publish log lines collapse to a single `release already live`.
    // ---------------------------------------------------------------------
    #[test]
    fn keep_existing_retouch_of_live_release_logs_already_live_only() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // An already-published (draft=false) release found by tag.
        let live = release_json(addr, 77, false, "v1.2.3");

        let routes = vec![
            // get_by_tag lookup finds the live release.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/tags/v1.2.3",
                response: http_ok(live.clone()),
                times: Some(1),
            },
            // PATCH the existing release (idempotent update).
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/o/r/releases/77",
                response: http_ok(live.clone()),
                times: None,
            },
        ];
        let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();

        // mode=keep-existing, draft=false (user wants the release live).
        let spec = GithubReleaseSpec {
            tag: "v1.2.3",
            name: "v1.2.3",
            body: "release body",
            mode: "keep-existing",
            draft: false,
            prerelease: false,
            make_latest: &None,
            target_commitish: &None,
            discussion_category: &None,
        };

        let (result, capture) = run_backend_capturing(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &spec,
            &base_opts(),
            &artifacts,
        );
        result.expect("backend succeeds").expect("returns Some");

        let messages: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            messages.iter().any(|m| m.contains("release already live")),
            "re-touch of a live release must log the concise already-live line; got: {messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.contains("created GitHub Release")),
            "re-touch must NOT re-emit the create line; got: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("published release")),
            "re-touch must NOT re-emit the publish line; got: {messages:?}"
        );
    }

    // ---------------------------------------------------------------------
    // 4. No artifacts — release is created but upload loop runs zero times.
    // ---------------------------------------------------------------------
    #[test]
    fn empty_artifacts_creates_release_but_uploads_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let routes = vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release_json(addr, 42, true, "v1.2.3")),
            times: Some(1),
        }];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();
        let anc = spec_ancillary_default();

        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("backend succeeds")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
            "must still POST create-release even with no artifacts; calls: {entries:?}",
        );
        assert!(
            !entries.iter().any(|e| e.path.starts_with("/upload/")),
            "empty artifact list must skip every upload POST; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 5. 422 already_exists + matching remote size → SkipIdempotent (no
    // delete, no error, success).
    // ---------------------------------------------------------------------
    #[test]
    fn upload_422_with_matching_remote_size_is_idempotent_skip() {
        let tmp = TempDir::new().expect("tempdir");
        let bytes = b"identical bytes";
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
        let artifact_len = bytes.len() as u64;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        let assets_page = format!("[{}]", asset_json(9, "demo.tar.gz", artifact_len));

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_422_already_exists(),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
                response: http_ok(assets_page),
                times: None,
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("422 + size match must succeed as SkipIdempotent")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            !entries.iter().any(|e| e.method == "DELETE"),
            "SkipIdempotent must NOT issue a DELETE; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 6. 422 already_exists + size mismatch + replace_existing_artifacts=false
    // → BailReplaceForbidden surfaces an error.
    // ---------------------------------------------------------------------
    #[test]
    fn upload_422_size_mismatch_without_replace_forbidden_bails() {
        let tmp = TempDir::new().expect("tempdir");
        let bytes = b"local content";
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        // Remote asset reports a DIFFERENT size (9999 vs local len).
        let assets_page = format!("[{}]", asset_json(9, "demo.tar.gz", 9999));

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_422_already_exists(),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
                response: http_ok(assets_page),
                times: None,
            },
        ];
        let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        // replace_existing_artifacts left false (default base_opts).
        let err = run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect_err("size-mismatch with replace=false must Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("replace_existing_artifacts: false")
                || msg.contains("already exists")
                || msg.contains("upload artifact"),
            "error must explain the conflict: {msg}",
        );
    }

    // ---------------------------------------------------------------------
    // 7. 422 already_exists + size mismatch + replace_existing_artifacts=true
    // → DeleteAndRetry succeeds on the second attempt.
    // ---------------------------------------------------------------------
    #[test]
    fn upload_422_size_mismatch_with_replace_allowed_deletes_and_retries() {
        let tmp = TempDir::new().expect("tempdir");
        let bytes = b"new content";
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
        let artifact_len = bytes.len() as u64;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        // First upload hits 422. The size probe returns 9999 (existing)
        // vs 11 (local) — classify_already_exists routes to
        // DeleteAndRetry, the stale asset_id=9 is deleted, and the
        // second upload succeeds.
        let stale_asset = asset_json(9, "demo.tar.gz", 9999);
        let stale_list = format!("[{stale_asset}]");

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            // Size-probe + recovery delete (size mismatch path,
            // triggered by the 422 below): GET assets returns the
            // stale asset; DELETE asset_id=9 clears the way; second
            // upload below succeeds.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
                response: http_ok(stale_list),
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/repos/o/r/releases/assets/9",
                response: HTTP_204,
                times: None,
            },
            // First upload attempt: 422.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_422_already_exists(),
                times: Some(1),
            },
            // Second upload attempt (after recovery delete): success.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(11, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

        let mut opts = base_opts();
        opts.replace_existing_artifacts = true;
        let anc = spec_ancillary_default();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &opts,
            &artifacts,
        )
        .expect("delete+retry must recover and succeed")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        let delete_count = entries
            .iter()
            .filter(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/assets/9")
            .count();
        assert!(
            delete_count >= 1,
            "replace_existing_artifacts=true must DELETE the stale asset at least once; calls: {entries:?}",
        );
        let upload_count = entries
            .iter()
            .filter(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
            .count();
        assert_eq!(
            upload_count, 2,
            "expected exactly 2 upload POSTs (first 422, second 201); calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 8. Missing token surfaces a clear error without any HTTP traffic.
    // ---------------------------------------------------------------------
    #[test]
    fn missing_token_errs_before_any_http_call() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Spawn the responder with no routes; ANY HTTP call lands in the
        // request log and fails the test.
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| Vec::new());

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token: Option<String> = None;
        let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();
        let anc = spec_ancillary_default();

        let err = run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect_err("missing token must Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("GITHUB_TOKEN") || msg.contains("token"),
            "error must mention the missing token: {msg}",
        );
        let entries = log.lock().expect("log mutex");
        assert!(
            entries.is_empty(),
            "token check must short-circuit BEFORE any HTTP call; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // 9. Missing artifact file surfaces a clear error after release create.
    // ---------------------------------------------------------------------
    #[test]
    fn missing_artifact_file_errs_with_path_in_message() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let routes = vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release_json(addr, 42, true, "v1.2.3")),
            times: Some(1),
        }];
        let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        // Point at a path that does not exist.
        let missing = PathBuf::from("/nonexistent/anodizer-test/does-not-exist.tar.gz");
        let artifacts = vec![(missing.clone(), Some("does-not-exist.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        let err = run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect_err("missing-on-disk artifact must Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing") && msg.contains("does-not-exist.tar.gz"),
            "missing-file error must name the offending path: {msg}",
        );
    }

    // ---------------------------------------------------------------------
    // 10. skip_upload = true creates the release but skips every upload POST.
    // ---------------------------------------------------------------------
    #[test]
    fn skip_upload_creates_release_but_skips_uploads() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"unused");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let routes = vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release_json(addr, 42, true, "v1.2.3")),
            times: Some(1),
        }];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

        let mut opts = base_opts();
        opts.skip_upload = true;
        let anc = spec_ancillary_default();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &opts,
            &artifacts,
        )
        .expect("backend succeeds")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            !entries.iter().any(|e| e.path.starts_with("/upload/")),
            "skip_upload=true must NOT POST any asset; calls: {entries:?}",
        );
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
            "skip_upload=true must still create the release; calls: {entries:?}",
        );
    }

    /// `404 Not Found` carrying a GitHub-shaped JSON body, so octocrab maps
    /// it to `Error::GitHub { status_code: 404 }` (the read-after-write lag
    /// shape) rather than a transport error.
    fn http_404() -> &'static str {
        let body = r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest"}"#;
        let len = body.len();
        Box::leak(
            format!("HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}")
                .into_boxed_str(),
        )
    }

    /// Force `retry.attempts: 1` to reproduce the stateful-mode policy
    /// (`--publish-only`), under which a single transient failure is
    /// otherwise unrecoverable. The readiness guard and the per-upload
    /// bounded-404 retry must both work despite this cap.
    fn build_ctx_attempts_one(addr: SocketAddr) -> Context {
        let mut ctx = build_ctx(addr);
        ctx.config.retry = Some(anodizer_core::config::RetryConfig {
            attempts: 1,
            delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        });
        ctx
    }

    // ---------------------------------------------------------------------
    // Post-create read-after-write lag: the readiness guard must absorb a
    // transient 404 on `GET /releases/{id}` before uploads start, even when
    // the resolved policy caps attempts at 1 (stateful `--publish-only`).
    // Without the guard the first `upload_asset` GET 404s and the run dies.
    // ---------------------------------------------------------------------
    #[test]
    fn readiness_guard_absorbs_transient_404_before_upload() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            // The readiness guard's first probe hits the replica before it
            // has observed the create: a transient 404 (served once).
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_404(),
                times: Some(1),
            },
            // Every subsequent GET (the guard's retry, then upload_asset's
            // own upload_url read) sees the release.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx_attempts_one(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("readiness guard must absorb the transient 404 and let the upload succeed")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz"),
            "the asset upload must reach the POST after the readiness guard recovers; calls: {entries:?}",
        );
    }

    // ---------------------------------------------------------------------
    // Backstop: even past the readiness guard, a parallel replica can lag
    // independently and 404 the `GET` inside `upload_asset(...).send()`. With
    // the stateful policy (attempts=1) that single 404 used to be fatal; the
    // per-upload bounded-404 floor must retry it instead.
    // ---------------------------------------------------------------------
    #[test]
    fn per_upload_404_retries_under_stateful_attempts_one() {
        let tmp = TempDir::new().expect("tempdir");
        let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
        let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");

        let routes = vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/o/r/releases",
                response: http_201(release.clone()),
                times: Some(1),
            },
            // (1) Readiness guard GET — readable on the first probe.
            // (2) upload_asset's upload_url GET on the FIRST attempt — 404
            //     (independent replica still lagging). attempts=1 would make
            //     this fatal without the per-upload bounded-404 floor.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release.clone()),
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_404(),
                times: Some(1),
            },
            // upload_asset's GET on the retry attempt, and any further reads.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/o/r/releases/42",
                response: http_ok(release),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/upload/42?name=demo.tar.gz",
                response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
                times: Some(1),
            },
        ];
        let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx_attempts_one(addr);
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
        let anc = spec_ancillary_default();

        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("per-upload bounded-404 retry must recover under attempts=1")
        .expect("returns Some");

        let entries = log.lock().expect("log mutex");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz"),
            "the asset upload must reach the POST after the per-upload 404 retry; calls: {entries:?}",
        );
    }
}
