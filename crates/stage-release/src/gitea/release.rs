use super::*;

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
        deadline: _,
        log,
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
    let existing =
        find_release_by_tag(client, api, &enc_owner, &enc_repo, tag, policy, log).await?;

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
            RetryLog::new("gitea: PATCH update release", log),
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
            RetryLog::new("gitea: POST create release", log),
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
pub(crate) async fn find_release_by_tag(
    client: &Client,
    api: &str,
    enc_owner: &str,
    enc_repo: &str,
    tag: &str,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<Option<(u64, Option<String>)>> {
    const MAX_PAGES: u32 = 10;
    const PAGE_SIZE: u32 = 50;

    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/api/v1/repos/{}/{}/releases?page={}&limit={}",
            api, enc_owner, enc_repo, page, PAGE_SIZE
        );

        let resp = retry_http_async(
            RetryLog::new(&format!("gitea: GET releases page {page}"), log),
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
