//! Gitea release backend — creates releases, uploads assets via the Gitea API.
//!
//! Gitea's release API is simpler than GitLab's: assets are uploaded directly
//! via multipart POST to the release endpoint (no package registry indirection).
//! Draft support is limited (Gitea has it but the release client treats
//! `PublishRelease` as a no-op), so we follow that same approach.
//!
//! Gitea release backend.
//!
//! ## Note on commit 4a9d25f (default-branch fallback)
//!
//! A `CreateFile` path previously hard-coded
//! `master` when the server-side default-branch lookup failed. Anodizer
//! does not call Gitea's `repos/{owner}/{repo}/contents/{path}` create-file
//! endpoint — every publisher (homebrew, scoop, krew, nix, aur, …) targets
//! Gitea via `git clone` + `git push` over SSH/HTTPS, not via the REST
//! contents API. The `branch`-defaulting bug therefore has no surface in
//! anodizer (n/a-by-construction).

use std::path::Path;

use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_async};
use anodizer_core::url::percent_encode_path_segment as encode_segment;
use anyhow::{Context as _, Result, bail};
use reqwest::Client;

use crate::release_body::compose_body_for_mode;

// ---------------------------------------------------------------------------
// Backend ctx + per-call specs
// ---------------------------------------------------------------------------
//
// Bundle the long argument lists in `gitea_create_release`,
// `gitea_upload_asset`, and `gitea_delete_asset_by_name` so each function
// signature stays under clippy's 7-argument threshold without an
// `#[allow(clippy::too_many_arguments)]` suppression. Mirrors gitlab.rs's
// `GitlabCtx`/`GitlabReleaseSpec`/`GitlabAssetSpec` shape.

/// Backend identity for a Gitea API call sequence.
///
/// Carries the HTTP client, base API URL, owner/repo coordinates, and retry
/// policy — i.e. everything that's constant for a whole release-publish
/// loop. Per-release fields (tag, name, body, …) live in
/// [`GiteaReleaseSpec`]; per-asset fields live in [`GiteaAssetSpec`].
#[derive(Clone, Copy)]
pub(crate) struct GiteaCtx<'a> {
    pub client: &'a Client,
    pub api_url: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub policy: &'a RetryPolicy,
}

/// Release metadata used by [`gitea_create_release`].
#[derive(Clone, Copy)]
pub(crate) struct GiteaReleaseSpec<'a> {
    pub tag: &'a str,
    pub commit: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub release_mode: &'a str,
}

/// File-on-disk identity used by every asset-upload call.
#[derive(Clone, Copy)]
pub(crate) struct GiteaAssetSpec<'a> {
    pub file_path: &'a Path,
    pub file_name: &'a str,
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Build the release page URL on the Gitea web UI.
///
/// Returns `{download}/{owner}/{repo}/releases/tag/{tag}`.
pub(crate) fn gitea_release_url(download_url: &str, owner: &str, repo: &str, tag: &str) -> String {
    let base = download_url.trim_end_matches('/');
    format!(
        "{}/{}/{}/releases/tag/{}",
        base,
        encode_segment(owner),
        encode_segment(repo),
        encode_segment(tag)
    )
}

/// Build a [`reqwest::Client`] configured for Gitea API access.
///
/// - `token`: the GITEA_TOKEN value.
/// - `skip_tls_verify`: when true, disable TLS certificate verification.
///
/// Gitea uses `Authorization: token {value}` for all API requests.
pub(crate) fn build_gitea_client(token: &str, skip_tls_verify: bool) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("token {}", token))
            .context("gitea: invalid token value for Authorization header")?,
    );

    let builder = Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(skip_tls_verify)
        .timeout(std::time::Duration::from_secs(300));

    builder.build().context("gitea: build HTTP client")
}

// ---------------------------------------------------------------------------
// Create / update release
// ---------------------------------------------------------------------------

/// Create or update a Gitea release.
///
/// Checks whether a release already exists for the given tag by listing
/// releases (paginated). If it exists, applies mode-based body composition
/// (keep-existing / append / prepend / replace) and updates via PATCH. If it
/// does not exist, creates via POST.
///
/// Returns the numeric release ID (Gitea uses integer IDs).
///
/// `ctx.policy` is the user-configured `Config.retry` block (or default 10 ×
/// 10s × 5m cap) — every HTTP call routes through [`retry_http_async`] so
/// 5xx / 429 / network-error responses retry with exponential backoff.
pub(crate) async fn gitea_create_release(
    ctx: &GiteaCtx<'_>,
    spec: &GiteaReleaseSpec<'_>,
) -> Result<u64> {
    let GiteaCtx {
        client,
        api_url,
        owner,
        repo,
        policy,
    } = *ctx;
    let GiteaReleaseSpec {
        tag,
        commit,
        name,
        body,
        draft,
        prerelease,
        release_mode,
    } = *spec;
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    // Gitea's `POST /repos/{owner}/{repo}/releases` requires non-empty
    // `tag_name`; `target_commitish` is required when the tag doesn't
    // already exist on the server (Gitea will create it at the given SHA).
    // Posting empty values surfaces as a 422 (`tag_name is required` /
    // `target_commitish is required`) that hides the real cause: the
    // tag template rendered empty or `ctx.git_info` was not populated.
    if tag.is_empty() {
        anyhow::bail!(
            "gitea: release for {}/{} is missing required tag_name. Gitea \
             POST /repos/{{owner}}/{{repo}}/releases rejects empty `tag_name`. \
             Verify the release tag template renders to a non-empty value \
             (e.g. `{{{{ Tag }}}}` is unset during `--snapshot`) or set an \
             explicit `release.tag:` override.",
            owner,
            repo
        );
    }
    if commit.is_empty() {
        anyhow::bail!(
            "gitea: release for {}/{} (tag '{}') is missing required \
             target_commitish (commit SHA). Gitea creates the tag at this \
             SHA when it doesn't already exist; empty values are rejected. \
             This means the git stage did not populate `ctx.git_info.commit` \
             — re-run `task release` from inside the git working tree so \
             git porcelain can resolve HEAD, or supply the SHA via the \
             upstream pipeline.",
            owner,
            repo,
            tag
        );
    }

    // Try to find an existing release by listing all releases and matching tag.
    let existing = find_release_by_tag(client, api, &enc_owner, &enc_repo, tag, policy).await?;

    if let Some((release_id, existing_body)) = existing {
        // Release exists — update it with mode-based body composition.
        let final_body = compose_body_for_mode(release_mode, existing_body.as_deref(), body);

        let update_url = format!(
            "{}/api/v1/repos/{}/{}/releases/{}",
            api, enc_owner, enc_repo, release_id
        );
        let payload = serde_json::json!({
            "tag_name": tag,
            "target_commitish": commit,
            "name": name,
            "body": final_body,
            "draft": draft,
            "prerelease": prerelease,
        });

        retry_http_async(
            "gitea: PATCH update release",
            policy,
            SuccessClass::Strict,
            |_| client.patch(&update_url).json(&payload).send(),
            |status, body| {
                format!(
                    "gitea: update release failed (HTTP {status}): {}",
                    redact_bearer_tokens(body)
                )
            },
        )
        .await?;

        Ok(release_id)
    } else {
        // Release does not exist — create it.
        let create_url = format!("{}/api/v1/repos/{}/{}/releases", api, enc_owner, enc_repo);
        let payload = serde_json::json!({
            "tag_name": tag,
            "target_commitish": commit,
            "name": name,
            "body": body,
            "draft": draft,
            "prerelease": prerelease,
        });

        let resp = retry_http_async(
            "gitea: POST create release",
            policy,
            SuccessClass::Strict,
            |_| client.post(&create_url).json(&payload).send(),
            |status, body| {
                format!(
                    "gitea: create release failed (HTTP {status}): {}",
                    redact_bearer_tokens(body)
                )
            },
        )
        .await?;

        let json: serde_json::Value = resp
            .json()
            .await
            .context("gitea: parse create release response JSON")?;

        let release_id = json["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("gitea: create release response missing 'id' field"))?;

        Ok(release_id)
    }
}

/// Find an existing release by tag name.
///
/// Iterates through paginated release listings (capped at 10 pages to avoid
/// runaway pagination on repos with very long release histories). This is
/// an intentional improvement: the listing paginates rather than truncating;
/// and only checks the first page of results.
///
/// Returns `Some((release_id, body))` if found, `None` otherwise.
async fn find_release_by_tag(
    client: &Client,
    api: &str,
    enc_owner: &str,
    enc_repo: &str,
    tag: &str,
    policy: &RetryPolicy,
) -> Result<Option<(u64, Option<String>)>> {
    const MAX_PAGES: u32 = 10;
    const PAGE_SIZE: u32 = 50;

    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/api/v1/repos/{}/{}/releases?page={}&limit={}",
            api, enc_owner, enc_repo, page, PAGE_SIZE
        );

        let resp = retry_http_async(
            &format!("gitea: GET releases page {page}"),
            policy,
            SuccessClass::Strict,
            |_| client.get(&url).send(),
            |status, body| {
                format!(
                    "gitea: list releases failed (HTTP {status}): {}",
                    redact_bearer_tokens(body)
                )
            },
        )
        .await?;

        let releases: Vec<serde_json::Value> = resp
            .json()
            .await
            .context("gitea: parse releases list JSON")?;

        for release in &releases {
            if release["tag_name"].as_str() == Some(tag) {
                let id = release["id"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("gitea: release missing 'id' field"))?;
                let body = release["body"].as_str().map(|s| s.to_string());
                return Ok(Some((id, body)));
            }
        }

        // If we got fewer results than the page size, there are no more pages.
        if releases.len() < PAGE_SIZE as usize {
            break;
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Upload asset
// ---------------------------------------------------------------------------

/// Upload a file as a release attachment via Gitea's multipart API.
///
/// ```text
/// POST {api}/api/v1/repos/{owner}/{repo}/releases/{id}/assets?name={filename}
/// Content-Type: multipart/form-data
/// ```
///
/// The file is sent as the `attachment` form field.
pub(crate) async fn gitea_upload_asset(
    ctx: &GiteaCtx<'_>,
    release_id: u64,
    asset: &GiteaAssetSpec<'_>,
) -> Result<()> {
    let GiteaCtx {
        client,
        api_url,
        owner,
        repo,
        policy,
    } = *ctx;
    let GiteaAssetSpec {
        file_path,
        file_name,
    } = *asset;
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);
    let enc_filename = encode_segment(file_name);

    let upload_url = format!(
        "{}/api/v1/repos/{}/{}/releases/{}/assets?name={}",
        api, enc_owner, enc_repo, release_id, enc_filename
    );

    let data = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("gitea: read file {}", file_path.display()))?;

    // Multipart Form is move-only — rebuild per attempt from the cloned
    // body bytes. `mime_str("application/octet-stream")` is structurally
    // infallible (a valid RFC-2045 token); same pattern as gitlab.rs and
    // cloudsmith.rs::retry_request.
    retry_http_async(
        "gitea: POST upload asset",
        policy,
        SuccessClass::Strict,
        |_| {
            let file_part = match reqwest::multipart::Part::bytes(data.clone())
                .file_name(file_name.to_string())
                .mime_str("application/octet-stream")
            {
                Ok(p) => p,
                Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
            };
            let form = reqwest::multipart::Form::new().part("attachment", file_part);
            client.post(&upload_url).multipart(form).send()
        },
        |status, body| {
            format!(
                "gitea: upload asset '{}' to release {} failed (HTTP {status}): {}",
                file_name,
                release_id,
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;

    Ok(())
}

/// Delete an existing release attachment by name.
///
/// Lists the release's attachments, finds one matching `file_name`, and
/// deletes it. Used for `replace_existing_artifacts` support.
pub(crate) async fn gitea_delete_asset_by_name(
    ctx: &GiteaCtx<'_>,
    release_id: u64,
    file_name: &str,
) -> Result<bool> {
    let GiteaCtx {
        client,
        api_url,
        owner,
        repo,
        policy,
    } = *ctx;
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    // List attachments for the release.
    let list_url = format!(
        "{}/api/v1/repos/{}/{}/releases/{}/assets",
        api, enc_owner, enc_repo, release_id
    );

    let resp = retry_http_async(
        "gitea: GET release assets",
        policy,
        SuccessClass::Strict,
        |_| client.get(&list_url).send(),
        |status, body| {
            format!(
                "gitea: list release assets failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;

    let assets: Vec<serde_json::Value> = resp
        .json()
        .await
        .context("gitea: parse release assets JSON")?;

    for asset in &assets {
        if asset["name"].as_str() == Some(file_name) {
            let asset_id = asset["id"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("gitea: asset missing 'id' field"))?;

            let delete_url = format!(
                "{}/api/v1/repos/{}/{}/releases/{}/assets/{}",
                api, enc_owner, enc_repo, release_id, asset_id
            );

            retry_http_async(
                "gitea: DELETE asset",
                policy,
                SuccessClass::Strict,
                |_| client.delete(&delete_url).send(),
                |status, body| {
                    format!(
                        "gitea: delete asset '{}' (id={}) from release {} failed (HTTP {status}): {}",
                        file_name,
                        asset_id,
                        release_id,
                        redact_bearer_tokens(body)
                    )
                },
            )
            .await?;

            return Ok(true);
        }
    }

    Ok(false)
}

/// What to do with a release asset whose name already exists on the remote,
/// decided from the size probe + the `replace_existing_artifacts` flag. The
/// same-size-skip-regardless-of-flag invariant comes from the shared
/// [`classify_asset_conflict`](crate::classify_asset_conflict); this enum is the
/// Gitea-specific projection of that decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GiteaUploadAction {
    /// Remote bytes match the local file: skip the upload (idempotent no-op).
    SkipIdempotent,
    /// Remote asset differs and the user opted into replacement: delete it,
    /// then re-upload.
    DeleteThenUpload,
    /// No matching same-size remote asset to skip and no opt-in delete to do:
    /// proceed straight to the upload.
    Upload,
}

/// Decide the upload action for a release asset.
///
/// The same-size idempotent skip fires REGARDLESS of
/// `replace_existing_artifacts` (matching the GitHub backend): a
/// byte-identical asset is a no-op, not an overwrite, so the user's
/// `replace_existing_artifacts: false` does not block it. The
/// delete-then-reupload only fires when the user opted in AND a
/// different-size remote asset actually exists.
///
/// Routes through the shared
/// [`classify_asset_conflict`](crate::classify_asset_conflict): the size probe
/// returns `Some` only when a same-named asset exists, so `remote_size.is_some()`
/// is the `remote_present` signal. A differing remote with overwrites forbidden
/// proceeds to `Upload` (Gitea has no pre-upload bail; the API surfaces the
/// conflict), matching the prior behaviour.
pub(crate) fn gitea_upload_action(
    replace_existing_artifacts: bool,
    remote_size: Option<u64>,
    local_size: u64,
) -> GiteaUploadAction {
    match crate::classify_asset_conflict(
        replace_existing_artifacts,
        remote_size.is_some(),
        remote_size,
        local_size,
    ) {
        crate::AssetConflict::IdenticalSkip => GiteaUploadAction::SkipIdempotent,
        crate::AssetConflict::ReplaceDiffering => GiteaUploadAction::DeleteThenUpload,
        crate::AssetConflict::ConflictForbidden | crate::AssetConflict::NoConflict => {
            GiteaUploadAction::Upload
        }
    }
}

/// Look up an existing release attachment by name and return its byte size.
///
/// Mirrors the GitHub backend's `find_release_asset_size`. Used by the
/// preemptive-delete path's idempotency check: when the remote asset's
/// size matches the local file, the upload is treated as an idempotent
/// no-op so the published bytes are not mutated (immutable-
/// releases policy).
pub(crate) async fn gitea_find_asset_size(
    ctx: &GiteaCtx<'_>,
    release_id: u64,
    file_name: &str,
) -> Result<Option<u64>> {
    let GiteaCtx {
        client,
        api_url,
        owner,
        repo,
        policy,
    } = *ctx;
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    let list_url = format!(
        "{}/api/v1/repos/{}/{}/releases/{}/assets",
        api, enc_owner, enc_repo, release_id
    );

    let resp = retry_http_async(
        "gitea: GET release assets (size probe)",
        policy,
        SuccessClass::Strict,
        |_| client.get(&list_url).send(),
        |status, body| {
            format!(
                "gitea: list release assets failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;

    let assets: Vec<serde_json::Value> = resp
        .json()
        .await
        .context("gitea: parse release assets JSON")?;

    for asset in &assets {
        if asset["name"].as_str() == Some(file_name) {
            // Gitea returns `size` as a 64-bit integer on the asset
            // payload. Missing/non-numeric is treated as "unknown size"
            // and falls through to delete-and-reupload.
            return Ok(asset["size"].as_u64());
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Backend orchestration
// ---------------------------------------------------------------------------

/// Runtime / context infrastructure for [`run_gitea_backend`].
///
/// Bundles the four "ambient" handles every backend call needs (matches the
/// shape of `github::BackendEnv`) so the function signature stays under
/// clippy's 7-argument threshold.
pub(crate) struct GiteaBackendEnv<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub ctx: &'a anodizer_core::context::Context,
    pub log: &'a anodizer_core::log::StageLogger,
    pub token: &'a Option<String>,
}

/// Per-release inputs the orchestrator forwards from `ReleaseStage::run` to
/// [`run_gitea_backend`]. Bundled so the function signature stays under
/// clippy's 7-argument threshold without an attribute suppression.
#[derive(Clone, Copy)]
pub(crate) struct GiteaBackendSpec<'a> {
    pub tag: &'a str,
    pub release_name: &'a str,
    pub release_body: &'a str,
    pub release_mode: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub use_existing_draft: bool,
    pub replace_existing_artifacts: bool,
}

/// Run the Gitea release backend for one crate.
///
/// Returns `(release_html_url, download_base, owner, repo_name)` on success,
/// or `Ok(None)` when the crate has no `release.gitea` (or fallback
/// `release.github`) configuration — callers should `continue` the outer
/// loop after this helper logs the "no gitea config" warning.
pub(crate) fn run_gitea_backend(
    env: &GiteaBackendEnv<'_>,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    spec: &GiteaBackendSpec<'_>,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
) -> Result<Option<(String, String, String, String)>> {
    use std::sync::Arc;

    let GiteaBackendEnv {
        rt,
        ctx,
        log,
        token,
    } = env;
    let ctx = *ctx;
    let log = *log;
    let token = *token;

    let repo_cfg = match crate::resolve_release_repo(release_cfg, ctx.token_type, ctx)? {
        Some(r) => r,
        None => {
            log.warn(&format!(
                "no gitea config for crate '{}', skipping",
                crate_cfg.name
            ));
            return Ok(None);
        }
    };

    let token_str = match token {
        Some(t) => t.clone(),
        None => {
            bail!("release: no Gitea token available (set GITEA_TOKEN, or pass --token)");
        }
    };

    let gitea_urls = ctx.config.gitea_urls.clone().unwrap_or_default();
    let api_url = gitea_urls
        .api
        .unwrap_or_else(|| "https://gitea.com/api/v1".to_string());
    let download_url = gitea_urls
        .download
        .unwrap_or_else(|| "https://gitea.com".to_string());
    let skip_tls = gitea_urls.skip_tls_verify.unwrap_or(false);

    let commit_sha = ctx
        .git_info
        .as_ref()
        .map(|g| g.commit.clone())
        .unwrap_or_default();

    // Gitea does not support draft releases robustly — warn if draft options are set.
    if spec.replace_existing_draft {
        log.warn("replace_existing_draft has no effect on Gitea (draft support is limited)");
    }
    if spec.use_existing_draft {
        log.warn("use_existing_draft has no effect on Gitea (draft support is limited)");
    }

    // Per-publisher retry policy. Same shape and rationale as GitLab.
    let policy = ctx.retry_policy();
    let tag = spec.tag;
    let release_name = spec.release_name;
    let release_body = spec.release_body;
    let release_mode = spec.release_mode;
    let skip_upload = spec.skip_upload;
    let replace_existing_artifacts = spec.replace_existing_artifacts;
    let draft = spec.draft;
    let prerelease = spec.prerelease;

    let url = rt.block_on(async {
        let client = build_gitea_client(&token_str, skip_tls)?;

        let gitea_ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: &repo_cfg.owner,
            repo: &repo_cfg.name,
            policy: &policy,
        };

        // Create or update the release.
        let release_id = gitea_create_release(
            &gitea_ctx,
            &GiteaReleaseSpec {
                tag,
                commit: &commit_sha,
                name: release_name,
                body: release_body,
                draft,
                prerelease,
                release_mode,
            },
        )
        .await?;

        log.status(&format!(
            "created Gitea Release '{}' (id={}, tag={}) on {}/{}",
            release_name, release_id, tag, repo_cfg.owner, repo_cfg.name
        ));

        // Upload artifacts with bounded parallelism (matching GitLab pattern).
        if skip_upload {
            log.status("skip_upload is set, skipping artifact uploads");
        } else {
            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
            let semaphore = Arc::new(tokio::sync::Semaphore::new(upload_parallelism));

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

            let client = Arc::new(client);
            let mut join_set = tokio::task::JoinSet::new();

            for (path, file_name) in prepared_entries {
                let sem = semaphore.clone();
                let client = client.clone();
                let api_url = api_url.clone();
                let owner = repo_cfg.owner.clone();
                let repo = repo_cfg.name.clone();
                let tag_owned = tag.to_string();
                let policy_inner = policy;

                join_set.spawn(async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                    let ctx = GiteaCtx {
                        client: &client,
                        api_url: &api_url,
                        owner: &owner,
                        repo: &repo,
                        policy: &policy_inner,
                    };

                    // Idempotency probe runs REGARDLESS of
                    // replace_existing_artifacts (matches the GitHub
                    // backend): probe the existing asset's byte size; when
                    // it matches the local file, skip the upload entirely so
                    // a re-run / --resume-release does NOT mutate
                    // already-published bytes and does NOT 409 on a duplicate
                    // filename. The probe is independent of the flag because
                    // a byte-identical asset is not an "overwrite" — the
                    // user's `replace_existing_artifacts: false` guards
                    // against replacing DIFFERENT bytes, not against a no-op.
                    let local_size = tokio::fs::metadata(&path)
                        .await
                        .with_context(|| {
                            format!(
                                "gitea: stat local artifact '{}' for size comparison",
                                file_name
                            )
                        })?
                        .len();
                    let remote_size = gitea_find_asset_size(&ctx, release_id, &file_name).await?;
                    match gitea_upload_action(replace_existing_artifacts, remote_size, local_size) {
                        GiteaUploadAction::SkipIdempotent => {
                            // Idempotent no-op: a prior attempt uploaded
                            // byte-identical content. Skip the upload.
                            return Ok::<String, anyhow::Error>(file_name);
                        }
                        GiteaUploadAction::DeleteThenUpload => {
                            gitea_delete_asset_by_name(&ctx, release_id, &file_name)
                                .await
                                .with_context(|| {
                                    format!(
                                        "gitea: delete existing asset '{}' from release {}",
                                        file_name, release_id
                                    )
                                })?;
                        }
                        GiteaUploadAction::Upload => {}
                    }

                    let op_name = format!("gitea: upload '{}'", file_name);
                    let asset = GiteaAssetSpec {
                        file_path: &path,
                        file_name: &file_name,
                    };
                    crate::retry_upload(&op_name, || gitea_upload_asset(&ctx, release_id, &asset))
                        .await
                        .with_context(|| {
                            format!(
                                "release: upload artifact '{}' to Gitea release '{}'",
                                file_name, tag_owned
                            )
                        })?;

                    Ok::<String, anyhow::Error>(file_name)
                });
            }

            while let Some(result) = join_set.join_next().await {
                let file_name = result
                    .context("gitea: upload task panicked")?
                    .context("gitea: upload task failed")?;
                log.verbose(&format!("uploaded artifact: {}", file_name));
            }
        }

        // Gitea PublishRelease is a no-op.

        let html_url = gitea_release_url(&download_url, &repo_cfg.owner, &repo_cfg.name, tag);
        Ok::<String, anyhow::Error>(html_url)
    })?;

    Ok(Some((
        url,
        download_url,
        repo_cfg.owner.clone(),
        repo_cfg.name.clone(),
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- gitea_release_url --------------------------------------------------

    #[test]
    fn release_url_basic() {
        let url = gitea_release_url("https://gitea.example.com", "myorg", "myapp", "v1.0.0");
        assert_eq!(
            url,
            "https://gitea.example.com/myorg/myapp/releases/tag/v1.0.0"
        );
    }

    #[test]
    fn release_url_trailing_slash_stripped() {
        let url = gitea_release_url("https://gitea.example.com/", "org", "repo", "v2.0.0");
        assert_eq!(
            url,
            "https://gitea.example.com/org/repo/releases/tag/v2.0.0"
        );
    }

    #[test]
    fn release_url_special_chars_in_tag() {
        let url = gitea_release_url(
            "https://gitea.example.com",
            "myorg",
            "myapp",
            "v1.0.0+build.1",
        );
        assert_eq!(
            url,
            "https://gitea.example.com/myorg/myapp/releases/tag/v1.0.0%2Bbuild.1"
        );
    }

    #[test]
    fn release_url_special_chars_in_owner_and_repo() {
        let url = gitea_release_url("https://gitea.example.com", "my org", "my repo", "v1.0.0");
        assert!(url.contains("my%20org"), "owner should be percent-encoded");
        assert!(url.contains("my%20repo"), "repo should be percent-encoded");
    }

    // -- encode_segment -----------------------------------------------------

    #[test]
    fn encode_segment_simple() {
        assert_eq!(encode_segment("v1.0.0"), "v1.0.0");
    }

    #[test]
    fn encode_segment_with_plus() {
        assert_eq!(encode_segment("v1.0.0+build.1"), "v1.0.0%2Bbuild.1");
    }

    #[test]
    fn encode_segment_with_special_chars() {
        assert_eq!(encode_segment("v1 beta#2?rc"), "v1%20beta%232%3Frc");
    }

    #[test]
    fn encode_segment_preserves_dots_dashes_underscores() {
        assert_eq!(encode_segment("my-project_v2.0"), "my-project_v2.0");
    }

    // -- gitea_upload_action ------------------------------------------------

    /// The same-size idempotent skip fires regardless of
    /// `replace_existing_artifacts` — a byte-identical asset is a no-op, not
    /// an overwrite. Without this, a re-run WITHOUT the flag would re-upload
    /// and 409 on the duplicate filename (the bug this fix closes).
    #[test]
    fn upload_action_same_size_skips_regardless_of_flag() {
        assert_eq!(
            gitea_upload_action(false, Some(100), 100),
            GiteaUploadAction::SkipIdempotent,
        );
        assert_eq!(
            gitea_upload_action(true, Some(100), 100),
            GiteaUploadAction::SkipIdempotent,
        );
    }

    /// Different-size remote asset + opt-in => delete then re-upload.
    #[test]
    fn upload_action_diff_size_with_flag_deletes() {
        assert_eq!(
            gitea_upload_action(true, Some(50), 100),
            GiteaUploadAction::DeleteThenUpload,
        );
    }

    /// Different-size remote asset WITHOUT opt-in => plain upload (no delete);
    /// Gitea surfaces its own duplicate-name behaviour. Preserves the prior
    /// no-flag fall-through.
    #[test]
    fn upload_action_diff_size_without_flag_uploads() {
        assert_eq!(
            gitea_upload_action(false, Some(50), 100),
            GiteaUploadAction::Upload,
        );
    }

    /// No remote asset at all => plain upload, and the opt-in delete is NOT
    /// attempted (nothing to delete) even with the flag set.
    #[test]
    fn upload_action_absent_remote_uploads_without_delete() {
        assert_eq!(
            gitea_upload_action(false, None, 100),
            GiteaUploadAction::Upload,
        );
        assert_eq!(
            gitea_upload_action(true, None, 100),
            GiteaUploadAction::Upload,
        );
    }

    // -- build_gitea_client -------------------------------------------------

    #[test]
    fn build_client_normal() {
        let client = build_gitea_client("giteatok-xxxx", false);
        assert!(client.is_ok());
    }

    #[test]
    fn build_client_skip_tls() {
        let client = build_gitea_client("giteatok-xxxx", true);
        assert!(client.is_ok());
    }

    // -- Gitea auth header format -------------------------------------------

    #[test]
    fn gitea_auth_header_format() {
        // A normal token forms a valid `token <value>` Authorization header, so
        // the client builds.
        assert!(build_gitea_client("my-gitea-token", false).is_ok());

        // A token carrying a control character cannot form a valid header value;
        // build_gitea_client must surface that as an error (with its context)
        // rather than panic on the internal HeaderValue::from_str.
        let err = build_gitea_client("bad\ntoken", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid token value"),
            "a control-char token must surface the Authorization header error: {err:#}"
        );
    }

    // -- gitea_create_release retry behaviour (P1.4) -------------------------
    //
    // Pin: a 503 on the find-release-by-tag GET must retry through
    // `retry_http_async` rather than fast-fail. Mirrors the gitlab equivalent
    // and the core retry::tests::retry_http_async_retries_5xx_then_succeeds
    // test, but exercises the policy plumbing end-to-end at the publisher.

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    #[tokio::test]
    async fn gitea_create_release_retries_5xx_on_list_releases() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        // Sequence: 503 on the GET releases list, then 200 with an empty
        // array (release does not exist), then 201 on the POST create with
        // a fake id. The retry helper should retry past the 503 and the
        // create succeeds.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"id\":42}",
        ]);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let api_url = format!("http://{addr}");

        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "myorg",
            repo: "myrepo",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc123",
            name: "Release v1.0.0",
            body: "release body",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };
        let result = gitea_create_release(&ctx, &spec).await;

        match result {
            Ok(id) => assert_eq!(id, 42, "release id should be parsed from create response"),
            Err(e) => panic!("expected success after 5xx retry, got: {e:#}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 3 connections (503-retry GET, 200 GET, 201 POST)"
        );
    }

    /// Defense-in-depth: a Gitea API 4xx response that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. Exercises the
    /// `find_release_by_tag` GET error path on the 401-fast-fail path.
    /// All gitea.rs body-interpolation sites share the same redaction wrap.
    #[tokio::test]
    async fn gitea_create_release_redacts_bearer_in_error_body() {
        use std::time::Duration;

        let leaky = r#"{"message":"401 Unauthorized: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
        let body_len = leaky.len();
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let api_url = format!("http://{addr}");

        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "myorg",
            repo: "myrepo",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc123",
            name: "Release v1.0.0",
            body: "release body",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };
        let err = gitea_create_release(&ctx, &spec)
            .await
            .expect_err("401 must fast-fail");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "bearer token leaked into error chain: {chain}"
        );
        assert!(
            chain.contains("<redacted>"),
            "expected `<redacted>` marker in error chain: {chain}"
        );
    }

    #[tokio::test]
    async fn gitea_release_tag_empty_bails_with_actionable_error() {
        // Gitea's `POST /repos/{owner}/{repo}/releases` rejects empty
        // `tag_name` with a vague 422; the helper must bail upfront
        // (before listing existing releases) so users see the real
        // cause. Bail message must name owner/repo and include an
        // actionable hint about the snapshot/template state.
        use std::time::Duration;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let ctx = GiteaCtx {
            client: &client,
            api_url: "http://unused.invalid",
            owner: "myorg",
            repo: "myrepo",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "",
            commit: "abc123",
            name: "Release",
            body: "body",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };
        let err = gitea_create_release(&ctx, &spec)
            .await
            .expect_err("empty tag must bail before any HTTP call");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("gitea:"),
            "error must carry the gitea: prefix, got: {chain}"
        );
        assert!(
            chain.contains("tag_name"),
            "error must name the rejected field, got: {chain}"
        );
        assert!(
            chain.contains("myorg/myrepo"),
            "error must name the owner/repo, got: {chain}"
        );
        assert!(
            chain.contains("release.tag:") || chain.contains("snapshot"),
            "error must include an actionable hint, got: {chain}"
        );
    }

    #[tokio::test]
    async fn gitea_release_commit_empty_bails_with_actionable_error() {
        // Gitea's create endpoint uses `target_commitish` to create the
        // tag when it doesn't already exist; empty values surface as a
        // 422. Bail upfront so users see that `ctx.git_info.commit` was
        // not populated.
        use std::time::Duration;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let ctx = GiteaCtx {
            client: &client,
            api_url: "http://unused.invalid",
            owner: "myorg",
            repo: "myrepo",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "",
            name: "Release",
            body: "body",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };
        let err = gitea_create_release(&ctx, &spec)
            .await
            .expect_err("empty commit must bail before any HTTP call");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("gitea:"),
            "error must carry the gitea: prefix, got: {chain}"
        );
        assert!(
            chain.contains("target_commitish"),
            "error must name the rejected field, got: {chain}"
        );
        assert!(
            chain.contains("git working tree") || chain.contains("git_info"),
            "error must include an actionable hint, got: {chain}"
        );
    }

    // -- HTTP release flow against the scripted responder -------------------
    //
    // The flat `spawn_oneshot_http_responder` above serves responses in
    // arrival order regardless of URL; it cannot assert WHICH endpoint each
    // call hit. These tests instead use the route-aware
    // `spawn_scripted_responder`, point `GiteaCtx.api_url` at
    // `http://{addr}`, and assert on the recorded request log: the exact
    // method/path/body of every create, find, update, upload, list, and
    // delete call the backend issues against Gitea's `/api/v1/...` surface.

    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder, spawn_scripted_responder_on,
    };

    /// Build a fast retry policy for the HTTP-flow tests (millisecond
    /// backoff so a retried 5xx doesn't stall the suite).
    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    /// Build a reqwest client with a short timeout for the HTTP-flow tests.
    fn test_client() -> Client {
        Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client")
    }

    /// Wrap a JSON body in a `200 OK` response with the right
    /// `Content-Length`. Leaks because the responder needs `&'static str`.
    fn http_json(status: &str, body: String) -> &'static str {
        let len = body.len();
        Box::leak(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
    }

    // -- gitea_create_release: create path (no existing release) ------------

    /// With no existing release on the first listing page, the backend
    /// POSTs to `.../releases` with the full create payload and parses the
    /// numeric `id` out of the 201 response.
    #[tokio::test]
    async fn create_release_posts_when_absent() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/myorg/myrepo/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/myorg/myrepo/releases",
                response: http_json("201 Created", serde_json::json!({"id": 99}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "myorg",
            repo: "myrepo",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "deadbeef",
            name: "Release v1.0.0",
            body: "the body",
            draft: true,
            prerelease: true,
            release_mode: "replace",
        };

        let id = gitea_create_release(&ctx, &spec)
            .await
            .expect("create should succeed");
        assert_eq!(id, 99, "release id parsed from POST 201 response");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "one GET list + one POST create");
        assert_eq!(entries[0].method, "GET");
        assert_eq!(entries[1].method, "POST");
        assert_eq!(
            entries[1].path, "/api/v1/repos/myorg/myrepo/releases",
            "create POSTs to the un-suffixed releases endpoint"
        );
        let payload: serde_json::Value =
            serde_json::from_str(&entries[1].body).expect("POST body is JSON");
        assert_eq!(payload["tag_name"], "v1.0.0");
        assert_eq!(payload["target_commitish"], "deadbeef");
        assert_eq!(payload["name"], "Release v1.0.0");
        assert_eq!(payload["body"], "the body");
        assert_eq!(payload["draft"], true);
        assert_eq!(payload["prerelease"], true);
    }

    /// A 422 from the create POST surfaces as an error (not a retry —
    /// `max_attempts: 1` proves the 4xx is fast-failed) and carries the
    /// gitea create-release context.
    #[tokio::test]
    async fn create_release_surfaces_422() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json(
                    "422 Unprocessable Entity",
                    serde_json::json!({"message": "tag already exists"}).to_string(),
                ),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc",
            name: "rel",
            body: "b",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };

        let err = gitea_create_release(&ctx, &spec)
            .await
            .expect_err("422 must surface as an error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("create release failed (HTTP 422"),
            "error must name the failing create call + status, got: {chain}"
        );
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "GET list + single POST (no retry on 4xx)");
    }

    /// A 503 on the POST create retries through `retry_http_async` and then
    /// succeeds on the second attempt; the request log records both POSTs.
    #[tokio::test]
    async fn create_release_retries_5xx_on_post() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(3);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc",
            name: "rel",
            body: "b",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };

        let id = gitea_create_release(&ctx, &spec)
            .await
            .expect("create should succeed after 5xx retry");
        assert_eq!(id, 7);
        let entries = log.lock().unwrap();
        let posts = entries.iter().filter(|e| e.method == "POST").count();
        assert_eq!(posts, 2, "503 POST retried once, then 201");
    }

    /// The create-response JSON missing an `id` field surfaces an
    /// explicit parse error rather than silently returning 0.
    #[tokio::test]
    async fn create_release_missing_id_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json(
                    "201 Created",
                    serde_json::json!({"name": "rel"}).to_string(),
                ),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc",
            name: "rel",
            body: "b",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };

        let err = gitea_create_release(&ctx, &spec)
            .await
            .expect_err("missing id must error");
        assert!(
            format!("{err:#}").contains("missing 'id' field"),
            "error must name the missing id field, got: {err:#}"
        );
    }

    // -- gitea_create_release: update path (existing release) ---------------

    /// When a release with the tag already exists, the backend PATCHes its
    /// numeric id and returns that id — no POST create is issued. The
    /// `replace` mode sends the new body verbatim.
    #[tokio::test]
    async fn update_release_patches_existing_replace_mode() {
        let existing = serde_json::json!([
            {"id": 5, "tag_name": "v1.0.0", "body": "old body"}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: http_json("200 OK", existing),
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/api/v1/repos/o/r/releases/5",
                response: http_json("200 OK", serde_json::json!({"id": 5}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc",
            name: "rel",
            body: "new body",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };

        let id = gitea_create_release(&ctx, &spec)
            .await
            .expect("update should succeed");
        assert_eq!(id, 5, "returns the existing release id");

        let entries = log.lock().unwrap();
        assert!(
            entries.iter().all(|e| e.method != "POST"),
            "existing release must be PATCHed, never POSTed"
        );
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("a PATCH was issued");
        assert_eq!(patch.path, "/api/v1/repos/o/r/releases/5");
        let payload: serde_json::Value =
            serde_json::from_str(&patch.body).expect("PATCH body is JSON");
        assert_eq!(
            payload["body"], "new body",
            "replace mode sends the new body verbatim"
        );
    }

    /// The `append` release mode composes the existing body and the new
    /// body into the PATCH payload (existing first, blank line, new).
    #[tokio::test]
    async fn update_release_append_mode_composes_body() {
        let existing = serde_json::json!([
            {"id": 8, "tag_name": "v2.0.0", "body": "EXISTING"}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: http_json("200 OK", existing),
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/api/v1/repos/o/r/releases/8",
                response: http_json("200 OK", serde_json::json!({"id": 8}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v2.0.0",
            commit: "abc",
            name: "rel",
            body: "ADDED",
            draft: false,
            prerelease: false,
            release_mode: "append",
        };

        gitea_create_release(&ctx, &spec)
            .await
            .expect("update should succeed");

        let entries = log.lock().unwrap();
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("a PATCH was issued");
        let payload: serde_json::Value =
            serde_json::from_str(&patch.body).expect("PATCH body is JSON");
        assert_eq!(
            payload["body"], "EXISTING\n\nADDED",
            "append mode joins existing + new with a blank line"
        );
    }

    /// A 503 on the PATCH update retries and then succeeds.
    #[tokio::test]
    async fn update_release_retries_5xx_on_patch() {
        let existing = serde_json::json!([
            {"id": 3, "tag_name": "v1.0.0", "body": null}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: http_json("200 OK", existing),
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/api/v1/repos/o/r/releases/3",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/api/v1/repos/o/r/releases/3",
                response: http_json("200 OK", serde_json::json!({"id": 3}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(3);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let spec = GiteaReleaseSpec {
            tag: "v1.0.0",
            commit: "abc",
            name: "rel",
            body: "b",
            draft: false,
            prerelease: false,
            release_mode: "replace",
        };

        let id = gitea_create_release(&ctx, &spec)
            .await
            .expect("update should succeed after 5xx retry");
        assert_eq!(id, 3);
        let entries = log.lock().unwrap();
        let patches = entries.iter().filter(|e| e.method == "PATCH").count();
        assert_eq!(patches, 2, "503 PATCH retried once, then 200");
    }

    // -- find_release_by_tag: pagination ------------------------------------

    /// A full first page (50 entries) that does not contain the tag forces
    /// a second GET; the match on page 2 returns its id + body without a
    /// third page request.
    #[tokio::test]
    async fn find_release_paginates_to_second_page() {
        let mut page1: Vec<serde_json::Value> = Vec::new();
        for i in 0..50u64 {
            page1.push(serde_json::json!({
                "id": 1000 + i,
                "tag_name": format!("other-{i}"),
                "body": null,
            }));
        }
        let page1_body = serde_json::Value::Array(page1).to_string();
        let page2_body = serde_json::json!([
            {"id": 4242, "tag_name": "v9.9.9", "body": "found me"}
        ])
        .to_string();

        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: http_json("200 OK", page1_body),
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=2&limit=50",
                response: http_json("200 OK", page2_body),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let found = find_release_by_tag(&client, &api_url, "o", "r", "v9.9.9", &policy)
            .await
            .expect("listing should succeed");
        assert_eq!(
            found,
            Some((4242, Some("found me".to_string()))),
            "tag matched on page 2 returns its id + body"
        );

        let entries = log.lock().unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/api/v1/repos/o/r/releases?page=1&limit=50",
                "/api/v1/repos/o/r/releases?page=2&limit=50",
            ],
            "exactly two pages fetched, in order"
        );
    }

    /// A short first page (fewer than `PAGE_SIZE` entries) with no match
    /// stops pagination and returns `None` — no second page is requested.
    #[tokio::test]
    async fn find_release_short_page_stops_and_returns_none() {
        let body = serde_json::json!([
            {"id": 1, "tag_name": "v0.1.0", "body": null}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", body),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let found = find_release_by_tag(&client, &api_url, "o", "r", "v2.0.0", &policy)
            .await
            .expect("listing should succeed");
        assert_eq!(found, None, "tag absent on a short page => None");

        let entries = log.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "a short first page must not trigger a second GET"
        );
    }

    /// A matched release object missing its `id` field surfaces an explicit
    /// error from the listing parse rather than a silent skip.
    #[tokio::test]
    async fn find_release_missing_id_errors() {
        let body = serde_json::json!([
            {"tag_name": "v1.0.0", "body": "no id here"}
        ])
        .to_string();
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", body),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let err = find_release_by_tag(&client, &api_url, "o", "r", "v1.0.0", &policy)
            .await
            .expect_err("matched-but-id-less release must error");
        assert!(
            format!("{err:#}").contains("release missing 'id' field"),
            "got: {err:#}"
        );
    }

    // -- gitea_upload_asset -------------------------------------------------

    /// Uploading an asset POSTs the file bytes (multipart) to
    /// `.../releases/{id}/assets?name={file}` and the request body carries
    /// the multipart `attachment` part + the file contents.
    #[tokio::test]
    async fn upload_asset_posts_multipart_to_assets_endpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("anodizer-x86_64.tar.gz");
        tokio::fs::write(&file, b"ARTIFACT-BYTES")
            .await
            .expect("write fixture");

        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/77/assets?name=anodizer-x86_64.tar.gz",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let asset = GiteaAssetSpec {
            file_path: &file,
            file_name: "anodizer-x86_64.tar.gz",
        };

        gitea_upload_asset(&ctx, 77, &asset)
            .await
            .expect("upload should succeed");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one upload POST");
        assert_eq!(entries[0].method, "POST");
        assert_eq!(
            entries[0].path, "/api/v1/repos/o/r/releases/77/assets?name=anodizer-x86_64.tar.gz",
            "name is carried in the query string, release id in the path"
        );
        assert!(
            entries[0].body.contains("name=\"attachment\""),
            "multipart body uses the `attachment` form field, got: {}",
            entries[0].body
        );
        assert!(
            entries[0].body.contains("ARTIFACT-BYTES"),
            "multipart body carries the file contents"
        );
    }

    /// A 503 on the asset POST retries (rebuilding the move-only multipart
    /// form per attempt) and then succeeds.
    #[tokio::test]
    async fn upload_asset_retries_5xx() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.bin");
        tokio::fs::write(&file, b"xyz")
            .await
            .expect("write fixture");

        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases/1/assets?name=a.bin",
                response: "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases/1/assets?name=a.bin",
                response: http_json("201 Created", serde_json::json!({"id": 2}).to_string()),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(3);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let asset = GiteaAssetSpec {
            file_path: &file,
            file_name: "a.bin",
        };

        gitea_upload_asset(&ctx, 1, &asset)
            .await
            .expect("upload should succeed after 5xx retry");
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "502 upload retried once, then 201");
    }

    /// A 4xx on the asset POST surfaces an error naming the asset, the
    /// release id, and the status.
    #[tokio::test]
    async fn upload_asset_surfaces_4xx() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.bin");
        tokio::fs::write(&file, b"xyz")
            .await
            .expect("write fixture");

        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/4/assets?name=a.bin",
            response: http_json(
                "400 Bad Request",
                serde_json::json!({"message": "bad asset"}).to_string(),
            ),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };
        let asset = GiteaAssetSpec {
            file_path: &file,
            file_name: "a.bin",
        };

        let err = gitea_upload_asset(&ctx, 4, &asset)
            .await
            .expect_err("400 must surface");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("upload asset 'a.bin' to release 4 failed (HTTP 400"),
            "error must name asset + release + status, got: {chain}"
        );
    }

    // -- gitea_delete_asset_by_name -----------------------------------------

    /// Deleting by name lists the release's assets, matches the name, then
    /// DELETEs `.../assets/{asset_id}` and returns `true`.
    #[tokio::test]
    async fn delete_asset_by_name_lists_then_deletes() {
        let assets = serde_json::json!([
            {"id": 11, "name": "other.bin", "size": 1},
            {"id": 22, "name": "target.bin", "size": 2}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/9/assets",
                response: http_json("200 OK", assets),
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/api/v1/repos/o/r/releases/9/assets/22",
                response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let deleted = gitea_delete_asset_by_name(&ctx, 9, "target.bin")
            .await
            .expect("delete should succeed");
        assert!(deleted, "matching asset reported as deleted");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "one list GET + one DELETE");
        assert_eq!(entries[0].method, "GET");
        assert_eq!(entries[1].method, "DELETE");
        assert_eq!(
            entries[1].path, "/api/v1/repos/o/r/releases/9/assets/22",
            "DELETE targets the matched asset's numeric id, not its name"
        );
    }

    /// When no listed asset matches the name, the backend issues no DELETE
    /// and returns `false`.
    #[tokio::test]
    async fn delete_asset_by_name_absent_returns_false() {
        let assets = serde_json::json!([
            {"id": 11, "name": "other.bin", "size": 1}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/9/assets",
            response: http_json("200 OK", assets),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let deleted = gitea_delete_asset_by_name(&ctx, 9, "missing.bin")
            .await
            .expect("listing should succeed");
        assert!(!deleted, "no match => false");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "only the list GET, no DELETE");
        assert!(entries.iter().all(|e| e.method != "DELETE"));
    }

    /// A 503 on the asset-list GET retries before the delete proceeds.
    #[tokio::test]
    async fn delete_asset_by_name_retries_5xx_on_list() {
        let assets = serde_json::json!([
            {"id": 33, "name": "t.bin", "size": 1}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/2/assets",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/2/assets",
                response: http_json("200 OK", assets),
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/api/v1/repos/o/r/releases/2/assets/33",
                response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(3);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let deleted = gitea_delete_asset_by_name(&ctx, 2, "t.bin")
            .await
            .expect("delete should succeed after list retry");
        assert!(deleted);
        let entries = log.lock().unwrap();
        let gets = entries.iter().filter(|e| e.method == "GET").count();
        assert_eq!(gets, 2, "503 list GET retried once before the DELETE");
        assert_eq!(entries.iter().filter(|e| e.method == "DELETE").count(), 1);
    }

    /// When the matched asset's DELETE returns a 4xx, the DELETE error closure
    /// fires: the function bails with a message naming the asset, its id, the
    /// release id, and the status — never returning `true`. `max_attempts: 1`
    /// proves the 4xx fast-fails rather than retrying.
    #[tokio::test]
    async fn delete_asset_by_name_surfaces_delete_failure() {
        let assets = serde_json::json!([
            {"id": 44, "name": "target.bin", "size": 7}
        ])
        .to_string();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/3/assets",
                response: http_json("200 OK", assets),
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/api/v1/repos/o/r/releases/3/assets/44",
                response: http_json(
                    "403 Forbidden",
                    serde_json::json!({"message": "no delete access"}).to_string(),
                ),
                times: None,
            },
        ]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let err = gitea_delete_asset_by_name(&ctx, 3, "target.bin")
            .await
            .expect_err("a 403 on the DELETE must surface as an error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("delete asset 'target.bin' (id=44) from release 3 failed (HTTP 403"),
            "error must name asset + id + release + status, got: {chain}"
        );
        let entries = log.lock().unwrap();
        assert_eq!(
            entries.iter().filter(|e| e.method == "DELETE").count(),
            1,
            "a 4xx DELETE fast-fails (no retry)"
        );
    }

    // -- gitea_find_asset_size ----------------------------------------------

    /// The size probe returns the matched asset's `size` field.
    #[tokio::test]
    async fn find_asset_size_returns_matched_size() {
        let assets = serde_json::json!([
            {"id": 1, "name": "a.bin", "size": 10},
            {"id": 2, "name": "b.bin", "size": 4096}
        ])
        .to_string();
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/5/assets",
            response: http_json("200 OK", assets),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let size = gitea_find_asset_size(&ctx, 5, "b.bin")
            .await
            .expect("probe should succeed");
        assert_eq!(size, Some(4096), "returns the matched asset's byte size");
    }

    /// The size probe returns `None` when no asset matches the name.
    #[tokio::test]
    async fn find_asset_size_absent_returns_none() {
        let assets = serde_json::json!([
            {"id": 1, "name": "a.bin", "size": 10}
        ])
        .to_string();
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/5/assets",
            response: http_json("200 OK", assets),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let size = gitea_find_asset_size(&ctx, 5, "missing.bin")
            .await
            .expect("probe should succeed");
        assert_eq!(size, None, "no name match => None");
    }

    /// A non-numeric / absent `size` field on the matched asset is treated
    /// as "unknown size" (`None`), which the caller maps to
    /// delete-and-reupload.
    #[tokio::test]
    async fn find_asset_size_non_numeric_size_is_none() {
        let assets = serde_json::json!([
            {"id": 1, "name": "a.bin", "size": "not-a-number"}
        ])
        .to_string();
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/5/assets",
            response: http_json("200 OK", assets),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(2);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let size = gitea_find_asset_size(&ctx, 5, "a.bin")
            .await
            .expect("probe should succeed");
        assert_eq!(
            size, None,
            "matched-but-unparseable size falls through to None"
        );
    }

    /// A 4xx on the size-probe asset-list GET fires the size-probe list error
    /// closure and bails (it is the size-probe variant of the list message,
    /// distinct from the delete path's list). `max_attempts: 1` proves the
    /// 4xx fast-fails.
    #[tokio::test]
    async fn find_asset_size_list_failure_surfaces_error() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/8/assets",
            response: http_json(
                "401 Unauthorized",
                serde_json::json!({"message": "bad token"}).to_string(),
            ),
            times: None,
        }]);

        let client = test_client();
        let policy = fast_policy(1);
        let api_url = format!("http://{addr}");
        let ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: "o",
            repo: "r",
            policy: &policy,
        };

        let err = gitea_find_asset_size(&ctx, 8, "a.bin")
            .await
            .expect_err("a 401 on the size-probe list must surface");
        assert!(
            format!("{err:#}").contains("list release assets failed (HTTP 401"),
            "error must name the failing list call + status, got: {err:#}"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "a 4xx list GET fast-fails (no retry)"
        );
    }

    // -- run_gitea_backend orchestration ------------------------------------
    //
    // These drive the production orchestrator (token resolution, URL
    // resolution, create-release, the per-asset idempotency probe +
    // delete-then-upload decision, and html_url composition) against the
    // scripted responder. The Context is built with token_type=Gitea so
    // `resolve_release_repo` reads `release.gitea`, and `gitea_urls.{api,
    // download}` point at the loopback so every API call is observable.
    // Mirrors the gitlab.rs `run_gitlab_backend` end-to-end tests.

    use anodizer_core::config::{
        CrateConfig, GiteaUrlsConfig, ReleaseConfig, RetryConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::scm::ScmTokenType;
    use anodizer_core::test_helpers::TestContextBuilder;

    /// Build a Gitea-flavoured Context: token_type=Gitea, a fast retry policy,
    /// and `gitea_urls.{api,download}` pointed at the loopback base so the URL
    /// builder's `/api/v1/...` suffix lands on the scripted responder.
    fn build_gitea_ctx(api_base: &str) -> Context {
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.0.0")
            .commit("deadbeef")
            .token(Some("gitea-test".to_string()))
            .build();
        ctx.token_type = ScmTokenType::Gitea;
        ctx.config.gitea_urls = Some(GiteaUrlsConfig {
            api: Some(api_base.to_string()),
            download: Some(api_base.to_string()),
            skip_tls_verify: None,
        });
        ctx.config.retry = Some(RetryConfig {
            attempts: 3,
            delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        });
        ctx
    }

    /// A `CrateConfig` whose `release.gitea` points at owner=o, name=r.
    fn build_gitea_crate_cfg() -> CrateConfig {
        let mut crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            ..Default::default()
        };
        crate_cfg.release = Some(ReleaseConfig {
            gitea: Some(ScmRepoConfig {
                owner: "o".to_string(),
                name: "r".to_string(),
            }),
            mode: Some("replace".to_string()),
            ..Default::default()
        });
        crate_cfg
    }

    fn default_gitea_spec() -> GiteaBackendSpec<'static> {
        GiteaBackendSpec {
            tag: "v1.0.0",
            release_name: "Release v1.0.0",
            release_body: "the body",
            release_mode: "replace",
            draft: false,
            prerelease: false,
            skip_upload: false,
            replace_existing_draft: false,
            use_existing_draft: false,
            replace_existing_artifacts: false,
        }
    }

    /// End-to-end: a fresh release (empty list GET → POST create) plus one
    /// asset whose size probe finds no remote match, so the upload proceeds.
    /// Asserts the success payload `(html_url, download, owner, repo)` and that
    /// the create POST, the size-probe GET, and the upload POST all hit the
    /// loopback.
    #[test]
    fn run_backend_creates_release_and_uploads_one_asset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("demo.tar.gz");
        std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
            // size probe: no assets yet => upload proceeds.
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
                response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
                times: None,
            },
        ];
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

        let out = run_gitea_backend(
            &env,
            &crate_cfg,
            release_cfg,
            &default_gitea_spec(),
            &artifacts,
        )
        .expect("run_gitea_backend should succeed")
        .expect("returns Some on success");
        let (html_url, download, owner, repo) = out;
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(
            download, api_base,
            "download base echoes gitea_urls.download"
        );
        assert_eq!(
            html_url,
            format!("{api_base}/o/r/releases/tag/v1.0.0"),
            "html_url composes from download base + owner/repo/releases/tag/tag"
        );

        let entries = log.lock().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/api/v1/repos/o/r/releases"),
            "the create POST hit the loopback"
        );
        let upload = entries
            .iter()
            .find(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz"))
            .expect("the upload POST was issued");
        assert!(
            upload.body.contains("PAYLOAD"),
            "the upload POST carried the artifact bytes"
        );
    }

    /// With `skip_upload` set, the orchestrator creates the release but issues
    /// no size probe and no upload POST.
    #[test]
    fn run_backend_skip_upload_creates_release_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("demo.tar.gz");
        std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
        ];
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let mut spec = default_gitea_spec();
        spec.skip_upload = true;
        let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

        run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
            .expect("run_gitea_backend should succeed")
            .expect("returns Some");

        let entries = log.lock().unwrap();
        assert!(
            entries.iter().all(|e| !e.path.contains("/assets")),
            "skip_upload must issue no size probe / upload calls, got: {:?}",
            entries.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
    }

    /// When the size probe finds a same-size remote asset, the upload is
    /// skipped (idempotent no-op): no DELETE, no upload POST — only the create
    /// flow plus the size probe GET.
    #[test]
    fn run_backend_idempotent_skip_when_size_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("demo.tar.gz");
        std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");
        let local_size = std::fs::metadata(&artifact).expect("stat").len();

        let existing_assets =
            serde_json::json!([{"id": 1, "name": "demo.tar.gz", "size": local_size}]).to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets",
                response: http_json("200 OK", existing_assets),
                times: None,
            },
        ];
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

        run_gitea_backend(
            &env,
            &crate_cfg,
            release_cfg,
            &default_gitea_spec(),
            &artifacts,
        )
        .expect("run_gitea_backend should succeed")
        .expect("returns Some");

        let entries = log.lock().unwrap();
        assert!(
            entries
                .iter()
                .all(|e| !(e.method == "POST" && e.path.contains("/assets?name="))),
            "a same-size remote asset must skip the upload POST entirely"
        );
        assert!(
            entries.iter().all(|e| e.method != "DELETE"),
            "an idempotent skip issues no DELETE"
        );
    }

    /// With `replace_existing_artifacts` and a DIFFERENT-size remote asset, the
    /// orchestrator deletes the conflicting asset (GET list + DELETE) and then
    /// re-uploads it (upload POST).
    #[test]
    fn run_backend_replace_existing_deletes_then_uploads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("demo.tar.gz");
        std::fs::write(&artifact, b"PAYLOAD-NEW-LONGER").expect("write artifact");

        // Remote reports a different size => DeleteThenUpload.
        let existing_assets =
            serde_json::json!([{"id": 5, "name": "demo.tar.gz", "size": 3}]).to_string();
        let list_again =
            serde_json::json!([{"id": 5, "name": "demo.tar.gz", "size": 3}]).to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
            // size probe (find differing size).
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets",
                response: http_json("200 OK", existing_assets),
                times: Some(1),
            },
            // delete-by-name list (matches the same asset id) ...
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets",
                response: http_json("200 OK", list_again),
                times: None,
            },
            // ... then the DELETE.
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets/5",
                response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            // ... then the re-upload.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
                response: http_json("201 Created", serde_json::json!({"id": 9}).to_string()),
                times: None,
            },
        ];
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let mut spec = default_gitea_spec();
        spec.replace_existing_artifacts = true;
        let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

        run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
            .expect("run_gitea_backend should succeed")
            .expect("returns Some");

        let entries = log.lock().unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(
                    |e| e.method == "DELETE" && e.path == "/api/v1/repos/o/r/releases/7/assets/5"
                )
                .count(),
            1,
            "the differing remote asset must be DELETEd before re-upload"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz"))
                .count(),
            1,
            "the asset is re-uploaded after the delete"
        );
    }

    /// Gitea's draft support is limited, so `replace_existing_draft` /
    /// `use_existing_draft` are no-ops that only emit a warning. With both set
    /// the orchestrator still creates the release and uploads the asset.
    #[test]
    fn run_backend_draft_flags_warn_but_create_proceeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("demo.tar.gz");
        std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
                response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
                times: None,
            },
        ];
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let mut spec = default_gitea_spec();
        spec.replace_existing_draft = true;
        spec.use_existing_draft = true;
        let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

        run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
            .expect("draft flags must not abort the backend")
            .expect("returns Some");

        let entries = log.lock().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path == "/api/v1/repos/o/r/releases"),
            "the release is still created despite the no-op draft flags"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz")),
            "the asset upload still proceeds"
        );
    }

    /// A missing Gitea token short-circuits before any HTTP call with an
    /// actionable bail naming GITEA_TOKEN.
    #[test]
    fn run_backend_missing_token_bails() {
        let ctx = build_gitea_ctx("http://unused.invalid");
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token: Option<String> = None;
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

        let err = run_gitea_backend(
            &env,
            &crate_cfg,
            release_cfg,
            &default_gitea_spec(),
            &artifacts,
        )
        .expect_err("a missing token must bail");
        assert!(
            format!("{err:#}").contains("GITEA_TOKEN"),
            "bail must name the missing env var, got: {err:#}"
        );
    }

    /// A crate without any `release.gitea`/`release.github` config returns
    /// `Ok(None)` (the caller `continue`s) rather than erroring.
    #[test]
    fn run_backend_no_gitea_config_returns_none() {
        let ctx = build_gitea_ctx("http://unused.invalid");
        let mut crate_cfg = build_gitea_crate_cfg();
        crate_cfg.release = Some(ReleaseConfig {
            mode: Some("replace".to_string()),
            ..Default::default()
        });
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

        let out = run_gitea_backend(
            &env,
            &crate_cfg,
            release_cfg,
            &default_gitea_spec(),
            &artifacts,
        )
        .expect("no-config is not an error");
        assert!(out.is_none(), "absent gitea config => Ok(None)");
    }

    /// A missing artifact file (path does not exist) aborts the upload loop
    /// with a "files are missing" error AFTER the release is created.
    #[test]
    fn run_backend_missing_artifact_file_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
                response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/repos/o/r/releases",
                response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
                times: None,
            },
        ];
        let (_addr, _log) = spawn_scripted_responder_on(listener, |_| routes);

        let api_base = format!("http://{addr}");
        let ctx = build_gitea_ctx(&api_base);
        let crate_cfg = build_gitea_crate_cfg();
        let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let log_stage = StageLogger::new("release", Verbosity::Normal);
        let token = Some("gitea-test".to_string());
        let env = GiteaBackendEnv {
            rt: &rt,
            ctx: &ctx,
            log: &log_stage,
            token: &token,
        };
        let missing = std::path::PathBuf::from("/nonexistent/anodizer-test/missing.tar.gz");
        let artifacts = vec![(missing, Some("missing.tar.gz".to_string()))];

        let err = run_gitea_backend(
            &env,
            &crate_cfg,
            release_cfg,
            &default_gitea_spec(),
            &artifacts,
        )
        .expect_err("a missing artifact file must abort the upload loop");
        assert!(
            format!("{err:#}").contains("missing"),
            "error must report the missing artifact, got: {err:#}"
        );
    }
}
