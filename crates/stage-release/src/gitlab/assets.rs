use super::*;

// ---------------------------------------------------------------------------
// Upload asset + create release link
// ---------------------------------------------------------------------------

/// Upload a file to GitLab and create a release link for it.
///
/// When `use_package_registry` is true (or when using job tokens), the file is
/// uploaded to the GitLab Generic Package Registry via PUT. Otherwise, it is
/// uploaded via the Project Markdown Uploads endpoint (POST multipart).
///
/// After the upload, a release link is created pointing to the uploaded file.
///
/// When `replace_existing` is true and the link creation returns HTTP 400/422
/// (duplicate), the existing link with the same name is deleted and the POST
/// is retried per the `replace_existing_artifacts` setting.
///
/// `ctx.policy` is the user-configured `Config.retry` block (or default 10 ×
/// 10s × 5m cap) — every HTTP call routes through [`retry_http_async`].
///
/// `pkg` selects the upload backend: `Some` routes through the Generic
/// Package Registry (PUT), `None` falls back to Project Markdown Uploads
/// (POST multipart) using `download_url` to construct the resulting link.
pub(crate) async fn gitlab_upload_asset(
    ctx: &GitlabCtx<'_>,
    tag: &str,
    asset: &GitlabAssetSpec<'_>,
    pkg: Option<&GitlabPackageRegistrySpec<'_>>,
    download_url: &str,
    replace_existing: bool,
) -> Result<()> {
    let GitlabCtx {
        client,
        api_url,
        project_id,
        policy,
        deadline,
        log,
    } = *ctx;
    let GitlabAssetSpec {
        file_path,
        file_name,
    } = *asset;
    let api = api_url.trim_end_matches('/');
    let encoded = encode_project_id(project_id);
    let encoded_tag = encode_tag(tag);

    let link_url = if let Some(pkg) = pkg {
        upload_via_package_registry(ctx, &encoded, asset, pkg).await?
    } else {
        upload_via_project_uploads(
            client,
            api,
            &encoded,
            file_path,
            file_name,
            download_url,
            policy,
            deadline,
            log,
        )
        .await?
    };

    // Create a release link for the uploaded asset.
    let links_api = format!(
        "{}/projects/{}/releases/{}/assets/links",
        api, encoded, encoded_tag
    );
    let direct_asset_path = format!("/{}", file_name);

    // Detect GitLab server version for the asset path field name.
    // GitLab v17+ uses `direct_asset_path`; older versions use `file_path`.
    let use_legacy_file_path = detect_pre_v17_gitlab(client, api_url).await;
    let path_field = if use_legacy_file_path {
        "filepath"
    } else {
        "direct_asset_path"
    };

    let payload = serde_json::json!({
        "name": file_name,
        "url": link_url,
        path_field: direct_asset_path,
    });

    // First attempt at creating the link. We don't use retry_http_async
    // directly here because the 400/422 "already exists" status is part of
    // the replace-existing control flow: those statuses are 4xx (would
    // fast-fail under the helper's classifier), but we want to react to
    // them by deleting the conflicting link and retrying.
    let resp = client
        .post(&links_api)
        .json(&payload)
        .send()
        .await
        .context("gitlab: POST create release link")?;

    let status_code = resp.status().as_u16();
    if resp.status().is_success() {
        return Ok(());
    }

    // If the link already exists (400/422) and replace_existing is enabled,
    // find and delete the conflicting link, then retry the POST.
    if (status_code == 400 || status_code == 422) && replace_existing {
        let text = anodizer_core::http::body_of(resp).await;
        // A failed cleanup (list or delete) surfaces the ORIGINAL create
        // failure as context — that is the actionable error — with the
        // cleanup failure chained underneath.
        if let Err(cleanup_err) = gitlab_delete_asset_link(ctx, tag, file_name).await {
            return Err(cleanup_err).with_context(|| {
                format!(
                    "gitlab: create release link for '{}' failed (HTTP {}): {}",
                    file_name,
                    status_code,
                    redact_bearer_tokens(&text)
                )
            });
        }

        // Retry the POST after deleting the conflicting link.
        retry_http_async(
            RetryLog::new("gitlab: POST create release link (retry after delete)", log),
            policy,
            SuccessClass::Strict,
            |_| client.post(&links_api).json(&payload).send(),
            |status, body| {
                format!(
                    "gitlab: create release link for '{}' failed on retry (HTTP {status}): {}",
                    file_name,
                    redact_bearer_tokens(body)
                )
            },
        )
        .await?;
    } else {
        let text = anodizer_core::http::body_of(resp).await;
        bail!(
            "gitlab: create release link for '{}' failed (HTTP {}): {}",
            file_name,
            status_code,
            redact_bearer_tokens(&text)
        );
    }

    Ok(())
}

/// Compose the release-links API URL for `tag`.
fn gitlab_links_api(api_url: &str, project_id: &str, tag: &str) -> String {
    format!(
        "{}/projects/{}/releases/{}/assets/links",
        api_url.trim_end_matches('/'),
        encode_project_id(project_id),
        encode_tag(tag)
    )
}

/// Look up the release link named `file_name` on the release for `tag`.
///
/// Returns `(link_id, url)` when a link with that exact name exists. The
/// list GET goes through `retry_http_async` so a transient 5xx doesn't
/// mis-report an existing link as absent.
pub(crate) async fn gitlab_find_asset_link(
    ctx: &GitlabCtx<'_>,
    tag: &str,
    file_name: &str,
) -> Result<Option<(u64, String)>> {
    let GitlabCtx {
        client,
        api_url,
        project_id,
        policy,
        deadline: _,
        log,
    } = *ctx;
    let links_api = gitlab_links_api(api_url, project_id, tag);
    let resp = retry_http_async(
        RetryLog::new("gitlab: GET existing release links", log),
        policy,
        SuccessClass::Strict,
        |_| client.get(&links_api).send(),
        |status, body| {
            format!(
                "gitlab: list existing release links failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;
    let links: Vec<serde_json::Value> = resp
        .json()
        .await
        .context("gitlab: parse release links JSON")?;
    for link in &links {
        if link["name"].as_str() == Some(file_name)
            && let Some(link_id) = link["id"].as_u64()
        {
            let url = link["url"].as_str().unwrap_or_default().to_string();
            return Ok(Some((link_id, url)));
        }
    }
    Ok(None)
}

/// Delete the release link named `file_name` on the release for `tag`.
///
/// Returns `Ok(true)` when a link was found and deleted, `Ok(false)` when no
/// link with that name exists. Deleting the link does not remove the linked
/// bytes (package-registry files / project uploads have no per-file delete on
/// this path); a subsequent upload replaces the link target.
pub(crate) async fn gitlab_delete_asset_link(
    ctx: &GitlabCtx<'_>,
    tag: &str,
    file_name: &str,
) -> Result<bool> {
    let Some((link_id, _url)) = gitlab_find_asset_link(ctx, tag, file_name).await? else {
        return Ok(false);
    };
    let GitlabCtx {
        client,
        api_url,
        project_id,
        policy,
        deadline: _,
        log,
    } = *ctx;
    let delete_url = format!("{}/{}", gitlab_links_api(api_url, project_id, tag), link_id);
    retry_http_async(
        RetryLog::new("gitlab: DELETE existing release link", log),
        policy,
        SuccessClass::Strict,
        |_| client.delete(&delete_url).send(),
        |status, body| {
            format!(
                "gitlab: delete existing link '{}' (id={}) failed (HTTP {status}): {}",
                file_name,
                link_id,
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;
    Ok(true)
}

/// Best-effort byte-size read of an already-linked asset: HEAD the link URL
/// with the authenticated probe client and read `Content-Length`.
///
/// The HEAD is issued ONLY when the link URL sits on one of the
/// `allowed_bases` hosts (the configured `api_url` / `download_url`): a
/// release link's URL is arbitrary attacker-influenceable data — any user
/// with release-write access can point one anywhere — and the client's
/// default headers carry the PRIVATE-TOKEN / JOB-TOKEN. An off-host link
/// returns `None` without any request. Pass a client built by
/// [`build_gitlab_probe_client`] (redirects off) so an on-host 302 to
/// external object storage cannot carry the token off-host either.
///
/// Returns `None` on any failure (off-host URL, non-2xx, missing/unparsable
/// header, transport error) — "present but size unknown" is a valid probe
/// verdict, so a probe miss must degrade gracefully rather than fail the
/// upload.
pub(crate) async fn gitlab_head_asset_size(
    client: &Client,
    url: &str,
    allowed_bases: &[&str],
) -> Option<u64> {
    if !link_url_on_configured_host(url, allowed_bases) {
        return None;
    }
    let resp = client.head(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// True when `url`'s scheme, host, and effective port all match at least
/// one of `allowed_bases`. Unparsable URLs on either side never match —
/// fail-closed, since a match authorizes sending the token to that host.
///
/// Scheme equality is required (not just host/port): without it,
/// `http://<host>:443/...` matches an `https://<host>/api/v4` base via
/// `port_or_known_default`, and the authenticated HEAD travels cleartext.
/// Strict equality (rather than only rejecting http-against-https) keeps
/// the rule symmetric and origin-shaped: a match means the exact
/// configured origin, nothing else.
pub(crate) fn link_url_on_configured_host(url: &str, allowed_bases: &[&str]) -> bool {
    let Ok(link) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(link_host) = link.host_str() else {
        return false;
    };
    allowed_bases.iter().any(|base| {
        reqwest::Url::parse(base).is_ok_and(|b| {
            b.scheme() == link.scheme()
                && b.host_str() == Some(link_host)
                && b.port_or_known_default() == link.port_or_known_default()
        })
    })
}

/// Upload a file via the GitLab Generic Package Registry.
///
/// ```text
/// PUT {api}/projects/{id}/packages/generic/{package}/{version}/{filename}
/// ```
///
/// `encoded_project_id` is passed in pre-encoded so the caller can amortize
/// the encoding across both upload paths in `gitlab_upload_asset`. `ctx`
/// provides the client / base URL / retry policy.
async fn upload_via_package_registry(
    ctx: &GitlabCtx<'_>,
    encoded_project_id: &str,
    asset: &GitlabAssetSpec<'_>,
    pkg: &GitlabPackageRegistrySpec<'_>,
) -> Result<String> {
    let GitlabCtx {
        client,
        api_url,
        policy,
        deadline,
        log,
        ..
    } = *ctx;
    let GitlabAssetSpec {
        file_path,
        file_name,
    } = *asset;
    let GitlabPackageRegistrySpec {
        project_name,
        version,
    } = *pkg;
    let api = api_url.trim_end_matches('/');
    let data = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("gitlab: read file {}", file_path.display()))?;

    let upload_url = format!(
        "{}/projects/{}/packages/generic/{}/{}/{}",
        api,
        encoded_project_id,
        encode_path_segment(project_name),
        encode_path_segment(version),
        encode_path_segment(file_name),
    );

    // Clone the body bytes per attempt — `RequestBuilder::body` consumes
    // them, and reqwest's reqwest::Body is move-only.
    retry_http_async_deadline(
        RetryLog::new("gitlab: PUT upload to package registry", log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| {
            client
                .put(&upload_url)
                .header("Content-Type", "application/octet-stream")
                .body(data.clone())
                .send()
        },
        |status, body| {
            format!(
                "gitlab: package registry upload '{}' failed (HTTP {status}): {}",
                file_name,
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;

    // The link URL for package registry assets is the same upload URL.
    Ok(upload_url)
}

/// Upload a file via the GitLab Project Markdown Uploads endpoint.
///
/// ```text
/// POST {api}/projects/{id}/uploads
/// Content-Type: multipart/form-data
/// ```
///
/// Returns the full download URL constructed from the download base URL and
/// the returned `full_path` field.
#[allow(clippy::too_many_arguments)]
async fn upload_via_project_uploads(
    client: &Client,
    api: &str,
    encoded_project_id: &str,
    file_path: &Path,
    file_name: &str,
    download_url: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &anodizer_core::log::StageLogger,
) -> Result<String> {
    let data = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("gitlab: read file {}", file_path.display()))?;

    let upload_url = format!("{}/projects/{}/uploads", api, encoded_project_id);

    // Multipart `Form` is move-only, so each retry attempt rebuilds it from
    // the cloned body bytes. `mime_str("application/octet-stream")` is
    // structurally infallible (a valid RFC-2045 token) so the error arm is
    // marked unreachable — same pattern as cloudsmith.rs::retry_request.
    let resp = retry_http_async_deadline(
        RetryLog::new("gitlab: POST project upload", log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| {
            let file_part = match reqwest::multipart::Part::bytes(data.clone())
                .file_name(file_name.to_string())
                .mime_str("application/octet-stream")
            {
                Ok(p) => p,
                Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
            };
            let form = reqwest::multipart::Form::new().part("file", file_part);
            client.post(&upload_url).multipart(form).send()
        },
        |status, body| {
            format!(
                "gitlab: project upload '{}' failed (HTTP {status}): {}",
                file_name,
                redact_bearer_tokens(body)
            )
        },
    )
    .await?;

    let body: serde_json::Value = resp
        .json()
        .await
        .context("gitlab: parse upload response JSON")?;

    // GitLab returns `{ "full_path": "/uploads/...", "url": "/uploads/...", ... }`.
    // Construct: `gitlabBaseURL + "/" + projectFile.FullPath`.
    // We follow the same simple approach.
    let full_path = body["full_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("gitlab: upload response missing 'full_path' field"))?;

    let base = download_url.trim_end_matches('/');
    let link = format!("{}/{}", base, full_path.trim_start_matches('/'));

    Ok(link)
}

// ---------------------------------------------------------------------------
// Forge-client face of the shared upload loop
// ---------------------------------------------------------------------------

/// The GitLab face of the shared upload loop
/// ([`crate::forge::run_upload_loop`]).
///
/// GitLab models release assets as *links*, so the probe checks the release's
/// link inventory: a link named like the artifact marks it present, and a
/// best-effort HEAD on the link URL supplies the byte size
/// ([`crate::forge::AssetPresence::Present`] with `size: None` when
/// unreadable). That makes a re-run / `--resume-release` skip byte-identical
/// uploads instead of re-uploading — the same idempotency the GitHub and
/// Gitea backends already had. Owns its coordinates so probe/delete/upload
/// futures are `'static`.
pub(crate) struct GitlabAssetClient {
    pub client: Client,
    /// Redirect-disabled sibling of `client` (same auth headers), used only
    /// for the HEAD size probe on link URLs — see
    /// [`build_gitlab_probe_client`] for why redirects must stay off there.
    pub probe_client: Client,
    pub api_url: String,
    pub project_id: String,
    pub policy: RetryPolicy,
    pub deadline: Option<std::time::Instant>,
    pub tag: String,
    pub download_url: String,
    /// `(project_name, version)` when uploads route through the Generic
    /// Package Registry; `None` = Project Markdown Uploads.
    pub pkg: Option<(String, String)>,
    pub replace_existing_artifacts: bool,
    pub log: anodizer_core::log::StageLogger,
}

impl GitlabAssetClient {
    fn api_ctx(&self) -> GitlabCtx<'_> {
        GitlabCtx {
            client: &self.client,
            api_url: &self.api_url,
            project_id: &self.project_id,
            policy: &self.policy,
            deadline: self.deadline,
            log: &self.log,
        }
    }
}

impl crate::forge::ForgeAssetClient for GitlabAssetClient {
    fn forge(&self) -> &'static str {
        "gitlab"
    }

    async fn before_uploads(&self, _entry_count: usize) -> Result<()> {
        Ok(())
    }

    async fn probe_asset(&self, file_name: &str) -> Result<crate::forge::AssetPresence> {
        Ok(
            match gitlab_find_asset_link(&self.api_ctx(), &self.tag, file_name).await? {
                Some((_link_id, url)) => crate::forge::AssetPresence::Present {
                    size: gitlab_head_asset_size(
                        &self.probe_client,
                        &url,
                        &[self.api_url.as_str(), self.download_url.as_str()],
                    )
                    .await,
                },
                None => crate::forge::AssetPresence::Absent,
            },
        )
    }

    async fn delete_asset(&self, file_name: &str) -> Result<()> {
        gitlab_delete_asset_link(&self.api_ctx(), &self.tag, file_name)
            .await
            .map(|_| ())
            .with_context(|| {
                format!(
                    "gitlab: delete existing release link '{}' on release '{}'",
                    file_name, self.tag
                )
            })
    }

    async fn upload_asset(
        &self,
        path: &Path,
        file_name: &str,
    ) -> Result<crate::forge::UploadOutcome> {
        let op_name = format!("gitlab: upload '{}'", file_name);
        let asset = GitlabAssetSpec {
            file_path: path,
            file_name,
        };
        let pkg_spec = self
            .pkg
            .as_ref()
            .map(|(project_name, version)| GitlabPackageRegistrySpec {
                project_name,
                version,
            });
        let api_ctx = self.api_ctx();
        crate::retry_upload(&op_name, &self.log, || {
            gitlab_upload_asset(
                &api_ctx,
                &self.tag,
                &asset,
                pkg_spec.as_ref(),
                &self.download_url,
                self.replace_existing_artifacts,
            )
        })
        .await
        .with_context(|| {
            format!(
                "release: upload artifact '{}' to GitLab release '{}'",
                file_name, self.tag
            )
        })?;
        Ok(crate::forge::UploadOutcome::Uploaded(file_name.to_string()))
    }
}
