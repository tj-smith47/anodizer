use super::*;

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
                "skipped release for crate '{}' — no github config",
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
            token: None,
        },
        None => github,
    };

    // Require a token for real API calls.
    let token_str = match token {
        Some(t) => t.clone(),
        None => {
            anyhow::bail!(
                "release: no GitHub token available ({})",
                anodizer_core::git::github_token_hint()
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
    // Absolute retry budget for this run (defaults to 15m, raisable via
    // `retry.max_elapsed`); threaded into the release-mutating octocrab calls
    // and the asset upload so a long 5xx/secondary-RL ladder can't outlive it.
    let deadline = ctx.retry_deadline();

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
            retry_octocrab_call(&policy, deadline, "delete release", Some(&retry_after_capture), || {
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

        // A release found by tag that is still a draft is, by anodizer's
        // draft-then-publish invariant, debris from an incomplete prior
        // attempt: a successful run always flips draft=false, and a draft's
        // assets are never publicly downloadable. Auto-resume into it
        // (overwrite same-name assets) so a CI retry self-heals without an
        // operator passing --resume-release. A *published* (draft=false)
        // release still blocks unless the user opts into replacement —
        // clobbering live, possibly-consumed artifacts must stay explicit.
        let existing_is_stale_draft = existing_by_tag.as_ref().is_some_and(|e| e.draft);
        let resume_release = resume_release || existing_is_stale_draft;
        let replace_existing_artifacts = replace_existing_artifacts || existing_is_stale_draft;

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
            retry_octocrab_call(&policy, deadline, "update draft release", Some(&retry_after_capture), || {
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
            retry_octocrab_call(&policy, deadline, "update existing release", Some(&retry_after_capture), || {
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
            retry_octocrab_call(&policy, deadline, "create release", Some(&retry_after_capture), || {
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

        // Re-touching an already-live release (the publish-pipeline pass that
        // runs after the release stage already created, uploaded, and
        // published it) must not re-POST every asset: each one comes back
        // 422 already_exists, and the redundant burst trips GitHub's
        // secondary rate limit. The assets are already attached, so skip the
        // upload loop entirely — UNLESS the operator asked for a real
        // overwrite (`--replace-existing` / `replace_existing_artifacts`),
        // in which case the loop runs and DELETEs-then-re-uploads each asset.
        let skip_upload = skip_upload || (retouch_live && !replace_existing_artifacts);

        // Upload artifacts (unless skip_upload is set), with bounded
        // parallelism using a semaphore (context's parallelism setting,
        // minimum 1).
        if skip_upload {
            if retouch_live {
                log.status("skipped artifact uploads — release already live with assets attached");
            } else {
                log.status("skipped artifact uploads — skip_upload is set");
            }
        } else {
            // Shared upload loop: concurrency-cap + pace resolution, the
            // missing-file bail, the bounded-parallel spawn, and the drain
            // all live in `forge::run_upload_loop`. GitHub's client probes
            // as `Reactive`, so the request sequence is exactly the
            // historical one (readiness guard, then upload POSTs with the
            // 422 `already_exists` recovery inside `upload_release_asset`).
            let plan = crate::forge::UploadPlan::resolve(
                release_cfg,
                env_source,
                replace_existing_artifacts,
            );
            let forge_client = Arc::new(super::super::upload::GithubAssetClient {
                octo: octo.clone(),
                owner: github.owner.clone(),
                repo: github.name.clone(),
                release_id: release_id_raw,
                tag: tag.to_string(),
                replace_existing_artifacts,
                policy,
                deadline,
                retry_after: retry_after_capture.clone(),
                token: token_str.clone(),
                env_source: Arc::clone(&env_source_arc),
                log: log.clone(),
            });
            crate::forge::run_upload_loop(forge_client, &plan, artifact_entries, log).await?;
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
                retry_octocrab_call(&policy, deadline, "publish PATCH", Some(&retry_after_capture), || {
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
                    "published release '{}' (draft → live)",
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
                let delete_result = retry_octocrab_call(&policy, deadline, "delete release (retention)", Some(&retry_after_capture), || {
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
                        retry_octocrab_call(&policy, deadline, "delete tag (retention)", Some(&retry_after_capture), || {
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
