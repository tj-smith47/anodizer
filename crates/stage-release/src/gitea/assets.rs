use super::*;

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
        deadline,
        log,
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
    retry_http_async_deadline(
        RetryLog::new("gitea: POST upload asset", log),
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
        deadline: _,
        log,
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
        RetryLog::new("gitea: GET release assets", log),
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
                RetryLog::new("gitea: DELETE asset", log),
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
/// Feeds [`GiteaAssetClient`]'s pre-upload probe in the shared upload loop:
/// when the remote asset's size matches the local file, the upload is an
/// idempotent no-op so the published bytes are not mutated
/// (immutable-releases policy).
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
        deadline: _,
        log,
    } = *ctx;
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    let list_url = format!(
        "{}/api/v1/repos/{}/{}/releases/{}/assets",
        api, enc_owner, enc_repo, release_id
    );

    let resp = retry_http_async(
        RetryLog::new("gitea: GET release assets (size probe)", log),
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
// Forge-client face of the shared upload loop
// ---------------------------------------------------------------------------

/// The Gitea face of the shared upload loop
/// ([`crate::forge::run_upload_loop`]).
///
/// Gitea exposes a listable asset inventory with byte sizes, so the probe is
/// proactive: the driver skips byte-identical re-uploads and pre-deletes an
/// opted-in overwrite before POSTing. Owns its coordinates (the `reqwest`
/// client is internally reference-counted) so probe/delete/upload futures are
/// `'static` and can move into spawned tasks.
pub(crate) struct GiteaAssetClient {
    pub client: Client,
    pub api_url: String,
    pub owner: String,
    pub repo: String,
    pub policy: RetryPolicy,
    pub deadline: Option<std::time::Instant>,
    pub release_id: u64,
    pub tag: String,
    pub log: anodizer_core::log::StageLogger,
}

impl GiteaAssetClient {
    fn api_ctx(&self) -> GiteaCtx<'_> {
        GiteaCtx {
            client: &self.client,
            api_url: &self.api_url,
            owner: &self.owner,
            repo: &self.repo,
            policy: &self.policy,
            deadline: self.deadline,
            log: &self.log,
        }
    }
}

impl crate::forge::ForgeAssetClient for GiteaAssetClient {
    fn forge(&self) -> &'static str {
        "gitea"
    }

    async fn before_uploads(&self, _entry_count: usize) -> Result<()> {
        Ok(())
    }

    /// [`gitea_find_asset_size`] returns `None` both for "no such asset" and
    /// for "asset present but size unreadable"; both map to `Absent` so the
    /// loop proceeds straight to the upload (the API surfaces any duplicate
    /// itself), preserving the pre-driver decision table.
    async fn probe_asset(&self, file_name: &str) -> Result<crate::forge::AssetPresence> {
        Ok(
            match gitea_find_asset_size(&self.api_ctx(), self.release_id, file_name).await? {
                Some(size) => crate::forge::AssetPresence::Present { size: Some(size) },
                None => crate::forge::AssetPresence::Absent,
            },
        )
    }

    async fn delete_asset(&self, file_name: &str) -> Result<()> {
        gitea_delete_asset_by_name(&self.api_ctx(), self.release_id, file_name)
            .await
            .map(|_| ())
            .with_context(|| {
                format!(
                    "gitea: delete existing asset '{}' from release {}",
                    file_name, self.release_id
                )
            })
    }

    async fn upload_asset(
        &self,
        path: &Path,
        file_name: &str,
    ) -> Result<crate::forge::UploadOutcome> {
        let op_name = format!("gitea: upload '{}'", file_name);
        let asset = GiteaAssetSpec {
            file_path: path,
            file_name,
        };
        let api_ctx = self.api_ctx();
        crate::retry_upload(&op_name, &self.log, || {
            gitea_upload_asset(&api_ctx, self.release_id, &asset)
        })
        .await
        .with_context(|| {
            format!(
                "release: upload artifact '{}' to Gitea release '{}'",
                file_name, self.tag
            )
        })?;
        Ok(crate::forge::UploadOutcome::Uploaded(file_name.to_string()))
    }
}
