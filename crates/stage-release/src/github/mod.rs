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
pub(crate) use rate_limit::check_github_rate_limit;
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
// dead-code anti-pattern rule. When a future consumer (e.g. changelog
// co-author enrichment in `stage-changelog/src/fetch/github.rs`) needs
// noreply parsing, re-introduce a focused helper in that crate's module.

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

/// Boolean cluster controlling upload semantics for [`run_github_backend`].
#[derive(Clone, Copy)]
pub(crate) struct UploadOpts {
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub replace_existing_artifacts: bool,
    pub use_existing_draft: bool,
    /// `--resume-release`: bypass the leftover-assets pre-check so the
    /// upload loop runs against an existing release left by a prior failed
    /// attempt.
    pub resume_release: bool,
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
    } = *upload_opts;
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
        check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

        // Handle replace_existing_draft: check if a draft release with
        // the same NAME exists and delete it.
        if replace_existing_draft
            && draft
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
                check_github_rate_limit(&rate_limit_client, &token_str, 10).await;
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
                let owner = github.owner.clone();
                let repo = github.name.clone();
                let tag_owned = tag.to_string();
                let lookup: Result<octocrab::models::repos::Release, octocrab::Error> =
                    retry_octocrab_call(&policy, "get release by tag", Some(&retry_after_capture), || {
                        let octo = octo.clone();
                        let owner = owner.clone();
                        let repo = repo.clone();
                        let tag_owned = tag_owned.clone();
                        async move {
                            octo.repos(&owner, &repo)
                                .releases()
                                .get_by_tag(&tag_owned)
                                .await
                        }
                    })
                    .await;
                match lookup {
                    Ok(existing) => {
                        let existing_body = existing.body.as_deref();
                        let body =
                            compose_body_for_mode(release_mode, existing_body, release_body);
                        (body, Some(existing))
                    }
                    Err(err) if is_octocrab_404(&err) => {
                        // A real 404 is the only non-error fall-through: no
                        // release exists for that tag, so the create-release
                        // POST below is the right next step. Every other
                        // status (auth, validation, exhausted retries on 5xx)
                        // propagates so the user sees the real GitHub error
                        // instead of a downstream 422 "tag already exists".
                        (release_body.to_string(), None)
                    }
                    Err(err) => {
                        return Err(anyhow::Error::new(err)).with_context(|| {
                            format!(
                                "release: look up existing release by tag '{}' on {}/{}",
                                tag, github.owner, github.name
                            )
                        });
                    }
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
        check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

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
            log.status(&format!(
                "updating existing release '{}' (id={}, mode={})",
                release_name, existing.id, release_mode
            ));
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

        log.status(&format!(
            "created GitHub Release '{}' (id={}) on {}/{}",
            release_name, release.id, github.owner, github.name
        ));

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
            let upload_concurrency: usize = std::env::var("ANODIZER_GITHUB_UPLOAD_CONCURRENCY")
                .ok()
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

            let mut join_set = tokio::task::JoinSet::new();

            for (path, file_name) in prepared_entries {
                let sem = semaphore.clone();
                let octo = octo.clone();
                let gh_owner = gh_owner.clone();
                let gh_name = gh_name.clone();
                let tag_c = tag_for_upload.clone();
                let token_for_rate_limit = token_str.clone();
                let retry_after_for_upload = retry_after_capture.clone();
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

                    // Handle replace_existing_artifacts: if an asset with the
                    // same name already exists, delete it before uploading.
                    // Uses paginated asset listing to handle releases with >30 assets.
                    if replace_existing_artifacts {
                        delete_release_asset_by_name(
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
                                "release: delete existing artifact '{}' from release '{}'",
                                file_name, tag_c
                            )
                        })?;
                    }

                    // Retry parameters come from `ctx.config.retry` (resolved
                    // into `policy` above): `attempts` caps the loop,
                    // `delay`/`max_delay` shape the exponential backoff. The
                    // loop body remains bespoke (resume-stream + 422
                    // already-exists handling); only the knobs are
                    // user-configurable. The `>= 1` clamp lives at
                    // `RetryConfig::to_policy` (see `RetryPolicy::max_attempts`
                    // rustdoc); no additional clamp is needed here.
                    let (max_upload_attempts, initial_retry_delay, max_retry_delay) =
                        upload_retry_locals(&policy);

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
                                check_github_rate_limit(
                                    &reqwest::Client::new(),
                                    &token_for_rate_limit,
                                    100,
                                )
                                .await;
                                last_err = Some(anyhow::anyhow!(err));
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
            check_github_rate_limit(&rate_limit_client, &token_str, 10).await;
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
            log.status(&format!(
                "published release '{}' (draft -> live)",
                release_name
            ));
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
    //! Pin the field surface of the three "context bundles" passed into
    //! `run_github_backend`. Each is `Clone + Copy` so we can construct a
    //! struct, copy it, and read every field through the copy — a future
    //! field removal/rename breaks compilation right here, not at the
    //! distant call site in `run.rs`.
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
    fn upload_opts_round_trips_every_boolean() {
        // Five independent booleans -> a drift in field order or a silent
        // removal would let the caller in `run.rs` send `replace_existing_draft`
        // where `skip_upload` was wanted. Pin each one by name.
        let opts = UploadOpts {
            skip_upload: true,
            replace_existing_draft: false,
            replace_existing_artifacts: true,
            use_existing_draft: false,
            resume_release: true,
        };
        let copy = opts; // exercises Copy
        assert!(copy.skip_upload);
        assert!(!copy.replace_existing_draft);
        assert!(copy.replace_existing_artifacts);
        assert!(!copy.use_existing_draft);
        assert!(copy.resume_release);
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
        };
        assert!(!opts.skip_upload);
        assert!(!opts.replace_existing_draft);
        assert!(!opts.replace_existing_artifacts);
        assert!(!opts.use_existing_draft);
        assert!(!opts.resume_release);
    }
}
