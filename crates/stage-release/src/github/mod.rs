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
mod retry_classify;

pub(crate) use assets::{delete_release_asset_by_name, find_release_asset_size};
pub(crate) use client::build_octocrab_client;
pub(crate) use rate_limit::check_github_rate_limit;
pub(crate) use retry_classify::classify_octocrab_error;
// NOTE: A `resolve_github_username` helper used to live alongside this mod
// (search-users API fallback for resolving commit author emails). Upstream
// removed the Search API call entirely in commit 17315a5 (parity item P3),
// leaving only the `users.noreply.github.com` pattern parser, which had no
// callers in anodizer. The whole module was deleted to satisfy the no-
// dead-code anti-pattern rule. When a future consumer (e.g. changelog
// co-author enrichment in `stage-changelog/src/fetch/github.rs`) needs
// noreply parsing, re-introduce a focused helper in that crate's module.

/// Run the GitHub release backend for one crate.
///
/// Returns:
/// - `Ok(Some((release_html_url, download_base, owner, repo)))` on success.
/// - `Ok(None)` when no `release.github` config is present for the crate
///   (callers should `continue` the outer loop with a warning already logged).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_github_backend(
    rt: &tokio::runtime::Runtime,
    ctx: &Context,
    log: &StageLogger,
    crate_cfg: &CrateConfig,
    release_cfg: &ReleaseConfig,
    token: &Option<String>,
    tag: &str,
    release_name: &str,
    release_body: &str,
    release_mode: &str,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
    draft: bool,
    prerelease: bool,
    skip_upload: bool,
    replace_existing_draft: bool,
    replace_existing_artifacts: bool,
    use_existing_draft: bool,
    make_latest: &Option<MakeLatest>,
    target_commitish: &Option<String>,
    discussion_category_name: &Option<String>,
    github_native_changelog: bool,
) -> Result<Option<(String, String, String, String)>> {
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

    // Build the octocrab instance and perform async API calls inside a
    // dedicated tokio runtime (the Stage trait is synchronous).
    let url = rt.block_on(async {
        let octo = build_octocrab_client(&token_str, &github_urls)?;
        let rate_limit_client = reqwest::Client::new();

        // Helper: list all releases (with pagination) and find a draft
        // matching the release name. GoReleaser searches by name (not tag).
        async fn find_draft_by_name(
            octo: &octocrab::Octocrab,
            owner: &str,
            repo: &str,
            name: &str,
        ) -> Result<Option<octocrab::models::repos::Release>> {
            // Cap at 10 pages (1000 releases) to avoid runaway pagination
            // on repos with very long release histories.
            const MAX_PAGES: u32 = 10;
            let mut page: u32 = 1;
            loop {
                let route = format!(
                    "/repos/{}/{}/releases?per_page=100&page={}",
                    owner, repo, page
                );
                let releases: Vec<octocrab::models::repos::Release> = octo
                    .get(route, None::<&()>)
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
                // If we got fewer than 100 results, there are no more pages.
                if releases.len() < 100 {
                    break;
                }
                page += 1;
                if page > MAX_PAGES {
                    break;
                }
            }
            Ok(None)
        }

        // Proactive rate limit check before draft search/release operations.
        check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

        // Handle replace_existing_draft: check if a draft release with
        // the same NAME exists and delete it.
        if replace_existing_draft
            && draft
            && let Some(existing) =
                find_draft_by_name(&octo, &github.owner, &github.name, release_name).await?
        {
            log.status(&format!(
                "replacing existing draft release '{}' (id={})",
                release_name, existing.id
            ));
            octo.repos(&github.owner, &github.name)
                .releases()
                .delete(existing.id.into_inner())
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
            match find_draft_by_name(&octo, &github.owner, &github.name, release_name).await? {
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
                match octo
                    .repos(&github.owner, &github.name)
                    .releases()
                    .get_by_tag(tag)
                    .await
                {
                    Ok(existing) => {
                        let existing_body = existing.body.as_deref();
                        let body =
                            compose_body_for_mode(release_mode, existing_body, release_body);
                        (body, Some(existing))
                    }
                    Err(_) => (release_body.to_string(), None),
                }
            } else {
                (release_body.to_string(), None)
            }
        };

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
        let json_body = build_release_json(
            tag,
            release_name,
            &final_body,
            true, // always create as draft first
            prerelease,
            &None, // make_latest deferred to publish PATCH
            target_commitish,
            &None, // discussion_category_name deferred to publish PATCH
            github_native_changelog,
        );

        // Rate limit check before release create/update API call.
        check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

        let release = if let Some(ref existing) = existing_draft {
            // Update the existing draft release via PATCH.
            let route = format!(
                "/repos/{}/{}/releases/{}",
                github.owner, github.name, existing.id
            );
            octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&json_body))
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
            octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&patch_body))
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
            octo.post::<_, octocrab::models::repos::Release>(route, Some(&json_body))
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

        // Wrap octo in Arc for shared use across parallel upload tasks
        // and the subsequent publish PATCH.
        let octo = Arc::new(octo);

        // Upload artifacts (unless skip_upload is set), with bounded
        // parallelism using a semaphore (context's parallelism setting,
        // minimum 1).
        if skip_upload {
            log.status("skip_upload is set, skipping artifact uploads");
        } else {
            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
            let semaphore = Arc::new(tokio::sync::Semaphore::new(upload_parallelism));
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
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "release: delete existing artifact '{}' from release '{}'",
                                file_name, tag_c
                            )
                        })?;
                    }

                    // Retry loop: up to 10 attempts with exponential backoff.
                    const MAX_UPLOAD_ATTEMPTS: u32 = 10;
                    const INITIAL_RETRY_DELAY: std::time::Duration =
                        std::time::Duration::from_millis(50);
                    const MAX_RETRY_DELAY: std::time::Duration =
                        std::time::Duration::from_secs(30);

                    let mut last_err: Option<anyhow::Error> = None;
                    // One-shot overwrite guard: once we've successfully deleted a
                    // stale asset and the upload *still* hits `already_exists`, give
                    // up gracefully instead of looping. This happens when GitHub's
                    // release-asset delete is eventually consistent — our delete
                    // returns Ok immediately but the subsequent upload still sees
                    // the stale asset for a short window. Rather than burn 10
                    // retries (and ultimately fail the whole release), accept the
                    // stale bytes and move on.
                    let mut overwrite_attempted = false;
                    for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
                        let data = std::fs::read(&path).with_context(|| {
                            format!("release: read artifact {}", path.display())
                        })?;
                        let local_size = data.len() as u64;

                        match octo
                            .repos(&gh_owner, &gh_name)
                            .releases()
                            .upload_asset(release_id_raw, &file_name, data.into())
                            .send()
                            .await
                        {
                            Ok(_) => {
                                last_err = None;
                                break;
                            }
                            Err(err) => {
                                let is_server_error = matches!(
                                    &err,
                                    octocrab::Error::GitHub { source, .. }
                                        if source.status_code.is_server_error()
                                );
                                // `already_exists` lives in GitHubError.errors[].code,
                                // not in the outer Display. octocrab::Error::GitHub's
                                // generated Display is just "GitHub" — inspect the
                                // source struct directly.
                                let is_already_exists = matches!(
                                    &err,
                                    octocrab::Error::GitHub { source, .. }
                                        if source.status_code.as_u16() == 422
                                            && source.errors.as_ref().is_some_and(|errs| {
                                                errs.iter().any(|e| {
                                                    e.get("code")
                                                        .and_then(|v| v.as_str())
                                                        == Some("already_exists")
                                                })
                                            })
                                );

                                if is_already_exists {
                                    // If we've already tried the delete+retry dance
                                    // once and upload *still* returns already_exists,
                                    // give up and keep the stale asset rather than
                                    // looping until MAX_UPLOAD_ATTEMPTS exhausts. The
                                    // re-appearing asset is typically a GitHub backend
                                    // eventual-consistency window after our prior
                                    // successful delete; retrying doesn't help.
                                    if overwrite_attempted {
                                        release_log().warn(&format!(
                                            "existing asset '{file_name}' on release '{tag_c}' \
                                             reappeared after delete+retry; \
                                             skipping — stale asset kept"
                                        ));
                                        last_err = None;
                                        break;
                                    }

                                    // Outer-retry idempotency: if an asset with the
                                    // same name already exists AND its size matches
                                    // the local artifact, a prior attempt in this
                                    // same release flow successfully uploaded it.
                                    // Treat as a no-op — the bytes GitHub has are
                                    // the bytes we intended to upload.
                                    let remote_size = find_release_asset_size(
                                        &octo,
                                        &gh_owner,
                                        &gh_name,
                                        release_id_raw,
                                        &file_name,
                                    )
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: look up existing asset '{}' on release '{}'",
                                            file_name, tag_c
                                        )
                                    })?;
                                    if remote_size == Some(local_size) {
                                        last_err = None;
                                        break;
                                    }

                                    // Size mismatch — overwrite if possible, else
                                    // skip gracefully. Always try to delete the stale
                                    // asset and retry; `replace_existing_artifacts` is
                                    // now the default behavior rather than an opt-in,
                                    // because failing the whole release on an asset
                                    // size mismatch is worse than replacing the stale
                                    // bytes (and the pipeline already has upstream
                                    // reproducibility gates for the cases where that
                                    // matters). If the delete itself fails (perms,
                                    // asset disappeared mid-flight, etc.), warn and
                                    // treat the upload as skipped — a stale asset is
                                    // better than aborting the release.
                                    match delete_release_asset_by_name(
                                        &octo,
                                        &gh_owner,
                                        &gh_name,
                                        release_id_raw,
                                        &file_name,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            overwrite_attempted = true;
                                            last_err = Some(anyhow::anyhow!(err));
                                            if attempt < MAX_UPLOAD_ATTEMPTS {
                                                let delay = std::cmp::min(
                                                    INITIAL_RETRY_DELAY * 2u32.pow(attempt - 1),
                                                    MAX_RETRY_DELAY,
                                                );
                                                tokio::time::sleep(delay).await;
                                            }
                                            continue;
                                        }
                                        Err(del_err) => {
                                            release_log().warn(&format!(
                                                "could not overwrite existing asset '{file_name}' on release '{tag_c}' \
                                                 (size mismatch and delete failed: {del_err}); skipping — stale asset kept"
                                            ));
                                            last_err = None;
                                            break;
                                        }
                                    }
                                }

                                // handle rate limiting
                                // (403/429) by sleeping and retrying.
                                let is_rate_limited = matches!(
                                    &err,
                                    octocrab::Error::GitHub { source, .. }
                                        if source.status_code.as_u16() == 403
                                            || source.status_code.as_u16() == 429
                                );

                                if is_rate_limited {
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
                                } else if is_server_error
                                    || matches!(&err, octocrab::Error::Hyper { .. })
                                    || matches!(&err, octocrab::Error::Http { .. })
                                    || matches!(&err, octocrab::Error::Service { .. })
                                    || matches!(&err, octocrab::Error::Other { .. })
                                    || matches!(&err, octocrab::Error::Serde { .. })
                                    || matches!(&err, octocrab::Error::Json { .. })
                                {
                                    // Transient transport / proxy issues during upload.
                                    // Serde / Json here means GitHub returned a non-JSON
                                    // body (typically an nginx/HAProxy 502/503 HTML page)
                                    // while our error-mapping expected JSON — always
                                    // transient, safe to retry. Log the variant so
                                    // future diagnostics don't have to guess.
                                    release_log().warn(&format!(
                                        "transient upload error on '{file_name}' attempt {attempt}/{MAX_UPLOAD_ATTEMPTS}: {err:?}"
                                    ));
                                    last_err = Some(anyhow::anyhow!(err));
                                    if attempt < MAX_UPLOAD_ATTEMPTS {
                                        let delay = std::cmp::min(
                                            INITIAL_RETRY_DELAY * 2u32.pow(attempt - 1),
                                            MAX_RETRY_DELAY,
                                        );
                                        tokio::time::sleep(delay).await;
                                    }
                                    continue;
                                } else {
                                    // Non-retryable error — fail immediately.
                                    return Err(anyhow::anyhow!(err)).with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to release '{}'",
                                            file_name, tag_c
                                        )
                                    });
                                }
                            }
                        }
                    }
                    if let Some(err) = last_err {
                        return Err(err).with_context(|| {
                            format!(
                                "release: upload artifact '{}' to release '{}' failed after {} attempts",
                                file_name, tag_c, MAX_UPLOAD_ATTEMPTS
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
            // Wire `Config.retry` (Wave 1 RetryConfig) into the publish PATCH:
            // GitHub occasionally 502s during un-draft if the release has many
            // assets attached, and the retry policy is the user-configurable
            // surface for this. Under-default (10 attempts × 10s base × 5m
            // cap) matches GoReleaser's `pkg/config.Retry` defaults.
            let policy = ctx.config.retry.unwrap_or_default().to_policy();
            anodizer_core::retry::retry_async(&policy, |attempt| {
                let publish_route = publish_route.clone();
                let publish_body = publish_body.clone();
                let octo = octo.clone();
                async move {
                    use std::ops::ControlFlow;
                    match octo
                        .patch::<octocrab::models::repos::Release, _, _>(
                            publish_route,
                            Some(&publish_body),
                        )
                        .await
                    {
                        Ok(release) => Ok(release),
                        Err(err) => {
                            // Shared classifier — same rule as the
                            // upload-asset retry above (Hyper / Http /
                            // Service / Other / Serde / Json all force-
                            // wrap into `Retriable` because their Display
                            // strings don't match `is_network_error`'s
                            // substring needles).
                            let (wrapped, status) = classify_octocrab_error(err);
                            if anodizer_core::retry::is_retriable(&*wrapped) {
                                release_log().warn(&format!(
                                    "release: publish PATCH failed (retriable, \
                                     attempt {attempt}, status={status})"
                                ));
                                Err(ControlFlow::Continue(anyhow::Error::from_boxed(
                                    wrapped,
                                )))
                            } else {
                                Err(ControlFlow::Break(anyhow::Error::from_boxed(wrapped)))
                            }
                        }
                    }
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
