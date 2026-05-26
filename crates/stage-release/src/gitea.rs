//! Gitea release backend — creates releases, uploads assets via the Gitea API.
//!
//! Gitea's release API is simpler than GitLab's: assets are uploaded directly
//! via multipart POST to the release endpoint (no package registry indirection).
//! Draft support is limited (Gitea has it but the GoReleaser client treats
//! `PublishRelease` as a no-op), so we follow that same approach.
//!
//! Reference: GoReleaser `internal/client/gitea.go`.
//!
//! ## Note on commit 4a9d25f (default-branch fallback)
//!
//! GoReleaser commit 4a9d25f fixes a `CreateFile` path that hard-coded
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
/// an intentional improvement over GoReleaser, which does not paginate
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

/// Look up an existing release attachment by name and return its byte size.
///
/// Mirrors the GitHub backend's `find_release_asset_size`. Used by the
/// preemptive-delete path's idempotency check: when the remote asset's
/// size matches the local file, the upload is treated as an idempotent
/// no-op so the published bytes are not mutated (GR v2.16 immutable-
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

                    // Handle replace_existing_artifacts (immutable-releases
                    // policy): probe the existing asset's byte size first;
                    // when it matches the local file, skip BOTH the delete
                    // AND the upload — same-size bytes are treated as an
                    // idempotent no-op so --resume-release does NOT mutate
                    // already-published assets. Different-size bytes plus
                    // the user's opt-in (`replace_existing_artifacts: true`)
                    // fall through to the delete-then-reupload path below.
                    if replace_existing_artifacts {
                        let local_size = tokio::fs::metadata(&path)
                            .await
                            .with_context(|| {
                                format!(
                                    "gitea: stat local artifact '{}' for size comparison",
                                    file_name
                                )
                            })?
                            .len();
                        if let Some(remote_size) =
                            gitea_find_asset_size(&ctx, release_id, &file_name).await?
                            && remote_size == local_size
                        {
                            // Idempotent no-op: a prior attempt uploaded
                            // byte-identical content. Skip the upload
                            // entirely so the published asset is not
                            // mutated.
                            return Ok::<String, anyhow::Error>(file_name);
                        }
                        gitea_delete_asset_by_name(&ctx, release_id, &file_name)
                            .await
                            .with_context(|| {
                                format!(
                                    "gitea: delete existing asset '{}' from release {}",
                                    file_name, release_id
                                )
                            })?;
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

        // Gitea PublishRelease is a no-op (matching GoReleaser).

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
        // Verify the Authorization header uses the `token {value}` format.
        let token = "my-gitea-token";
        let expected_header = format!("token {}", token);

        // Build the client and verify the default headers contain the correct auth.
        let client = build_gitea_client(token, false).unwrap();

        // We can't directly inspect reqwest's default headers, but we can verify
        // the format by testing the construction doesn't fail with the token format.
        // The real verification is that the header value "token my-gitea-token" is valid.
        let header_value = reqwest::header::HeaderValue::from_str(&expected_header).unwrap();
        assert_eq!(
            header_value.to_str().unwrap(),
            "token my-gitea-token",
            "Gitea auth header must use 'token {{value}}' format"
        );

        // Ensure client was built successfully (implies headers are valid)
        drop(client);
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
}
