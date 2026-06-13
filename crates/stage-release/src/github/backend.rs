//! The GitHub release orchestrator.
//!
//! [`run_github_backend`] is the body of the `ScmTokenType::GitHub` match arm
//! in the dispatcher loop: it resolves the repo + tag, creates / updates /
//! replaces the release, drives the parallel asset-upload loop with bounded
//! transient retry, publishes the release, and only then runs the
//! nightly-retention sweep (so the new release is live before any prior
//! release is pruned). The lookup, classifier, and client helpers it composes
//! live in the sibling [`super::lookup`], [`super::spec`], and the per-tool
//! helper submodules.

use std::sync::Arc;

use anodizer_core::config::{CrateConfig, ReleaseConfig};
use anyhow::{Context as _, Result};

use super::lookup::{
    find_draft_by_name, find_release_by_tag, list_releases_by_name, wait_for_release_readable,
};
use super::spec::{
    BackendEnv, GithubReleaseSpec, UploadOpts, check_existing_assets_block_upload,
    nightly_releases_to_prune, resolve_upload_pace,
};
use super::upload::{UploadAssetRequest, upload_release_asset};
use super::{
    build_octocrab_client, check_github_rate_limit_with_env, is_octocrab_404, retry_octocrab_call,
};
use crate::release_body::{
    GITHUB_RELEASE_BODY_MAX_CHARS, build_publish_patch_body, build_release_json,
    compose_body_for_mode,
};
use crate::resolve_release_repo;

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
    // Default download URL to "https://github.com".
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
        // draft is stale state to remove whether the upcoming release publishes
        // or re-drafts. `find_draft_by_name` only ever matches `r.draft` releases,
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

        // Nightly retention runs AFTER the new release is created, uploaded,
        // and published (see the sweep below the publish PATCH). Pruning before
        // creation is irreversible-before-reversible: a hard failure between the
        // delete and a live new release would leave zero published nightly with
        // `keep_last: 1`.

        // When updating an existing release, apply mode-based body composition.
        // Also track any existing release found by tag so it can be PATCHed
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

        // Create or update the release. Raw API calls are used for all paths
        // to support target_commitish and discussion_category_name, which
        // are not fully exposed by octocrab's builder API.
        //
        // Draft-then-publish: always create as draft first so users never
        // see a release with missing artifacts. After all uploads succeed,
        // a PATCH sets draft=false when the user wanted a non-draft release.
        let user_wants_draft = draft;
        // GitHub ignores discussion_category_name on draft releases and
        // make_latest is meaningless until publish. Send them only in the
        // un-draft PATCH (below).
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
                    "release '{}' already live (id={}, mode={})",
                    release_name, existing.id, release_mode
                ));
            }
            let route = format!(
                "/repos/{}/{}/releases/{}",
                github.owner, github.name, existing.id
            );
            // preserve the existing
            // release's draft state on PATCH. The default json_body is
            // built with `draft=true` for the create path; when updating
            // an existing release it must not flip back to draft.
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
        // owner/repo/tag.
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
            // Proactive upload pace: the minimum interval between successive
            // upload STARTS, layered on top of the concurrency cap and the
            // reactive secondary-rate-limit backoff. The concurrency cap alone
            // lets the first `upload_concurrency` POSTs fire in the same
            // instant — the exact burst that trips GitHub's secondary rate
            // limit; spacing each task's spawn by this interval smooths that
            // burst. `Duration::ZERO` means pacing is disabled (rely on the
            // cap + backoff). env `ANODIZER_GITHUB_UPLOAD_PACE_MS` >
            // `release.upload_pace` > 200 ms default.
            let upload_pace = resolve_upload_pace(release_cfg, env_source);
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

            for (idx, (path, file_name)) in prepared_entries.into_iter().enumerate() {
                // Proactive pace: space each upload START by at least
                // `upload_pace`, jittered ±20% so concurrent releases don't
                // synchronise their bursts. Skipped for the first task (no
                // prior start to space from) and when pacing is disabled
                // (`Duration::ZERO`). The semaphore still bounds how many run
                // at once; this only governs how fast new starts are admitted.
                if idx > 0 && !upload_pace.is_zero() {
                    tokio::time::sleep(anodizer_core::retry::jitter_duration(upload_pace)).await;
                }
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

                    upload_release_asset(UploadAssetRequest {
                        octo: &octo,
                        owner: &gh_owner,
                        repo: &gh_name,
                        release_id: release_id_raw,
                        tag: &tag_c,
                        path: &path,
                        file_name: &file_name,
                        replace_existing_artifacts,
                        policy: &policy_for_upload,
                        retry_after: Some(&retry_after_for_upload),
                        token: &token_for_rate_limit,
                        env_source: env_for_upload.as_ref(),
                    })
                    .await?;

                    Ok::<String, anyhow::Error>(file_name)
                });
            }

            // Collect results from all upload tasks.
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(file_name)) => {
                        log.verbose(&format!("uploaded artifact {}", file_name));
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
            // Build the publish PATCH body via the helper:
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
            // base, 5m cap) are the retry defaults.
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

        // Nightly retention sweep: keep the N newest nightly releases (matched
        // by the rendered nightly release name) and delete the rest, AFTER the
        // new release is created, uploaded, and published. Running it here
        // (rather than before creation) is irreversible-before-reversible: the
        // new release is live first, so a failure during the prune can never
        // leave zero published nightly. `keep_last: 1` is the
        // rolling-single-release case (the `keep_single_release` alias resolves
        // to it upstream); larger N keeps N. All route through the same prune
        // arithmetic ([`nightly_releases_to_prune`]) — no parallel path.
        //
        // Skipped when an existing-draft reuse is in play (the draft IS the
        // release that gets PATCHed).
        //
        // The just-created release id (`release_id_raw`) is passed as the
        // protected id so the prune arithmetic can never select the release this
        // run just published. Each pruned release's git ref is deleted too,
        // EXCEPT the current `tag` (the live release's own ref).
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
            let to_prune = nightly_releases_to_prune(&existing, keep_last, release_id_raw);
            for (rel_id, rel_tag) in to_prune {
                log.status(&format!(
                    "deleting prior release '{release_name}' (id={rel_id}, tag='{rel_tag}') for nightly retention (keep_last={keep_last})"
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
                            "prior release '{release_name}' (id={rel_id}) already deleted by a concurrent process (nightly retention)"
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
                // Delete the pruned release's git tag too, unless it is the live
                // release's own tag (which must stay intact).
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
                                "failed to delete stale tag '{rel_tag}' on {}/{} during nightly retention: {err}",
                                github.owner, github.name
                            ));
                        }
                    }
                }
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
    use anodizer_core::context::Context;
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

        // The retention sweep now runs AFTER the new release is created, so the
        // list-by-name returns the just-created release (id=42) alongside the
        // two existing nightly releases. Newest-first: 42, 11, 10. With
        // keep_last=2 the kept set is {42, 11}; id=10 + its tag "nightly.0" is
        // pruned. The new release id=42 must NEVER be pruned.
        let new_release = release_json_named(addr, 42, "demo-nightly", "v1.2.3");
        let list_body = format!(
            "[{},{},{}]",
            release_json_named(addr, 42, "demo-nightly", "v1.2.3"),
            release_json_named(addr, 11, "demo-nightly", "nightly.1"),
            release_json_named(addr, 10, "demo-nightly", "nightly.0"),
        );

        let routes = vec![
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
        // The just-created release (id=42) must NEVER be deleted by the sweep.
        assert!(
            !entries
                .iter()
                .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/42"),
            "the just-created release id=42 must never be pruned; calls: {entries:?}",
        );

        // M6 ordering: the new release must be created (and its asset uploaded)
        // BEFORE any retention delete fires. Pruning before the new release is
        // live is irreversible-before-reversible.
        let create_pos = entries
            .iter()
            .position(|e| e.method == "POST" && e.path == "/repos/o/r/releases")
            .expect("create POST must occur");
        let upload_pos = entries
            .iter()
            .position(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
            .expect("asset upload POST must occur");
        let first_delete_pos = entries
            .iter()
            .position(|e| e.method == "DELETE" && e.path.starts_with("/repos/o/r/releases/"))
            .expect("a retention delete must occur");
        assert!(
            create_pos < first_delete_pos,
            "release must be created before any retention delete; calls: {entries:?}",
        );
        assert!(
            upload_pos < first_delete_pos,
            "asset upload must complete before any retention delete; calls: {entries:?}",
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
            messages
                .iter()
                .any(|m| m == "release 'v1.2.3' already live (id=77, mode=keep-existing)"),
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

    // ---------------------------------------------------------------------
    // Proactive upload pace — the minimum interval between upload STARTS.
    // ---------------------------------------------------------------------

    /// Build a [`Context`] like [`build_ctx`] but also seed the
    /// `ANODIZER_GITHUB_UPLOAD_PACE_MS` override so the pace timing tests can
    /// drive the inter-upload-start interval without touching config.
    fn build_ctx_with_pace_ms(addr: SocketAddr, pace_ms: &str) -> Context {
        let base = format!("http://{addr}");
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.2.3")
            .token(Some("test-token".to_string()))
            .env("ANODIZER_GITHUB_API_BASE", &base)
            .env("ANODIZER_GITHUB_UPLOAD_PACE_MS", pace_ms)
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

    /// Route set for an N-asset happy-path upload against release id 42:
    /// create POST, a reusable GET on the release (readiness + per-upload
    /// `upload_url` read), and one upload POST per asset name.
    fn multi_asset_routes(release: String, names: &[(&'static str, u64)]) -> Vec<ScriptedRoute> {
        let mut routes = vec![
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
        ];
        for (name, id) in names {
            routes.push(ScriptedRoute {
                method: "POST",
                path_pattern: Box::leak(format!("/upload/42?name={name}").into_boxed_str()),
                response: http_201(asset_json(*id, name, 5)),
                times: Some(1),
            });
        }
        routes
    }

    /// With a non-zero pace, successive upload STARTS are spaced by at least
    /// the (jittered) pace interval. Three assets => two inter-start gaps, so
    /// total wall-clock must be at least `2 * pace * 0.8` (the jitter floor).
    /// A 120 ms pace yields a >= ~192 ms floor — comfortably above scheduler
    /// noise yet fast enough to keep the test cheap.
    #[test]
    fn upload_pace_spaces_successive_upload_starts() {
        use std::time::Instant;

        let tmp = TempDir::new().expect("tempdir");
        let a = write_artifact(tmp.path(), "a.tar.gz", b"aaaaa");
        let b = write_artifact(tmp.path(), "b.tar.gz", b"bbbbb");
        let c = write_artifact(tmp.path(), "c.tar.gz", b"ccccc");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");
        let routes = multi_asset_routes(
            release,
            &[("a.tar.gz", 1), ("b.tar.gz", 2), ("c.tar.gz", 3)],
        );
        let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx_with_pace_ms(addr, "120");
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![
            (a, Some("a.tar.gz".to_string())),
            (b, Some("b.tar.gz".to_string())),
            (c, Some("c.tar.gz".to_string())),
        ];
        let anc = spec_ancillary_default();

        let t0 = Instant::now();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("paced upload succeeds")
        .expect("returns Some");
        let elapsed = t0.elapsed();

        // 2 gaps * 120 ms * 0.8 jitter floor = 192 ms.
        assert!(
            elapsed >= std::time::Duration::from_millis(192),
            "upload pace must space the 3 starts by >= 2 * 120ms * 0.8; elapsed: {elapsed:?}"
        );
    }

    /// With pace disabled (`ANODIZER_GITHUB_UPLOAD_PACE_MS=0`) the upload loop
    /// must NOT insert any inter-start delay. The same three-asset upload that
    /// the paced test spaces to >= 192 ms here completes well under the pace
    /// floor — proving `0` is a true no-op (the concurrency cap + reactive
    /// backoff remain the only governors).
    #[test]
    fn upload_pace_zero_is_a_no_op() {
        use std::time::Instant;

        let tmp = TempDir::new().expect("tempdir");
        let a = write_artifact(tmp.path(), "a.tar.gz", b"aaaaa");
        let b = write_artifact(tmp.path(), "b.tar.gz", b"bbbbb");
        let c = write_artifact(tmp.path(), "c.tar.gz", b"ccccc");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let release = release_json(addr, 42, true, "v1.2.3");
        let routes = multi_asset_routes(
            release,
            &[("a.tar.gz", 1), ("b.tar.gz", 2), ("c.tar.gz", 3)],
        );
        let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let ctx = build_ctx_with_pace_ms(addr, "0");
        let crate_cfg = build_crate_cfg();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let token = Some("test-token".to_string());
        let artifacts = vec![
            (a, Some("a.tar.gz".to_string())),
            (b, Some("b.tar.gz".to_string())),
            (c, Some("c.tar.gz".to_string())),
        ];
        let anc = spec_ancillary_default();

        let t0 = Instant::now();
        run_backend(
            &rt,
            &ctx,
            &token,
            &crate_cfg,
            &make_spec(&anc),
            &base_opts(),
            &artifacts,
        )
        .expect("unpaced upload succeeds")
        .expect("returns Some");
        let elapsed = t0.elapsed();

        // No pacing: the only delays are loopback round-trips. Bound well
        // below the 192 ms paced floor so a regression that always paces
        // (ignoring the 0 sentinel) trips this assertion.
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "pace=0 must add no inter-start delay; elapsed: {elapsed:?}"
        );
    }
}
