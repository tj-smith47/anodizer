use super::*;

// ---------------------------------------------------------------------------
// Create / update release
// ---------------------------------------------------------------------------

/// Create or update a GitLab release.
///
/// Checks whether the release already exists for the given tag. If it does,
/// applies mode-based body composition (keep-existing / append / prepend /
/// replace) and updates via PUT. If it does not exist, creates via POST.
///
/// `policy` is the user-configured `Config.retry` block (or its default of 10
/// attempts × 10s base × 5m cap) — every HTTP call inside this function and
/// the asset-upload sibling routes through [`retry_http_async`] using this
/// policy so 5xx / 429 / network-error responses are retried with backoff
/// instead of failing fast.
///
/// Returns the tag name (GitLab's release identifier).
pub(crate) async fn gitlab_create_release(
    ctx: &GitlabCtx<'_>,
    spec: &GitlabReleaseSpec<'_>,
) -> Result<String> {
    let GitlabCtx {
        client,
        api_url,
        project_id,
        policy,
        deadline: _,
        log,
    } = *ctx;
    let GitlabReleaseSpec {
        tag,
        name,
        body,
        commit,
        release_mode,
    } = *spec;
    // GitLab's `POST /projects/:id/releases` requires non-empty `tag_name`.
    // The empty check is upfront (before the GET probe) because the probe
    // URL also bakes the tag into the path; an empty `encoded_tag` would
    // hit `/releases/` (the listing endpoint) and silently return 200, then
    // fall through to a POST create with `tag_name: ""` which GitLab 400s
    // (`tag_name can't be blank`). Bail with the real cause first.
    if tag.is_empty() {
        anyhow::bail!(
            "gitlab: release for project '{}' is missing required tag_name. \
             GitLab POST /projects/:id/releases rejects empty `tag_name` and \
             an empty path segment in the GET probe URL would silently hit \
             the listing endpoint, masking the bug. Verify the release tag \
             template renders to a non-empty value (e.g. `{{{{ Tag }}}}` is \
             unset during `--snapshot`) or set an explicit `release.tag:` \
             override.",
            project_id
        );
    }

    let api = api_url.trim_end_matches('/');
    let encoded = encode_project_id(project_id);
    let encoded_tag = encode_tag(tag);

    // Try to get the existing release for this tag. The success branch needs
    // to inspect status (403/404 = "create") so we cannot use Strict success
    // class here — instead, fast-fail on 4xx is unwanted for the GET probe;
    // we accept 403/404 as a legitimate "not found" signal. The simplest
    // correct shape is a manual classify: route 5xx + transport errors
    // through retry_http_async (success_class=Strict makes 4xx a Break),
    // catch the Break for 403/404, and treat it as the "create" branch.
    //
    // Concretely: try the GET; if it 4xx-fast-fails with 403/404, fall
    // through to the create-POST. Anything else propagates.
    let get_url = format!("{}/projects/{}/releases/{}", api, encoded, encoded_tag);
    let get_outcome = retry_http_async(
        RetryLog::new("gitlab: GET release by tag", log),
        policy,
        SuccessClass::Strict,
        |_| client.get(&get_url).send(),
        |status, body| {
            format!(
                "gitlab: GET release by tag failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )
    .await;

    let create_branch = match get_outcome {
        Ok(get_resp) => {
            // Release exists — update it with mode-based body composition.
            let existing: serde_json::Value = get_resp
                .json()
                .await
                .context("gitlab: parse existing release JSON")?;
            let existing_body = existing["description"].as_str();
            let final_body = compose_body_for_mode(release_mode, existing_body, body);

            let update_url = format!("{}/projects/{}/releases/{}", api, encoded, encoded_tag);
            let payload = serde_json::json!({
                "name": name,
                "description": final_body,
            });

            retry_http_async(
                RetryLog::new("gitlab: PUT update release", log),
                policy,
                SuccessClass::Strict,
                |_| client.put(&update_url).json(&payload).send(),
                |status, body| {
                    format!(
                        "gitlab: update release failed (HTTP {status}): {}",
                        redact_bearer_tokens(body)
                    )
                },
            )
            .await?;
            false
        }
        Err(err) => {
            // Inspect the chain for HttpError(403|404) — those are the
            // "release does not exist, create it" signal. Anything else
            // (5xx exhaustion, transport failure, other 4xx) propagates.
            let status_code = err
                .chain()
                .find_map(|e| {
                    e.downcast_ref::<anodizer_core::retry::HttpError>()
                        .map(|h| h.status)
                })
                .unwrap_or(0);
            if status_code == 403 || status_code == 404 {
                true
            } else {
                return Err(err);
            }
        }
    };

    if create_branch {
        // Release does not exist — create it. GitLab's create endpoint
        // requires non-empty `ref` (the commit SHA / branch the tag points
        // to). Empty `ref` produces a vague 400 (`ref is missing`) that
        // hides the real cause: `ctx.git_info` was not populated by the
        // git stage (e.g. running `release --snapshot` outside a git
        // working tree). The empty-`tag_name` case is already guarded
        // upfront above; only the commit check is branch-local because
        // the existing-release PUT update path does not send `ref`.
        if commit.is_empty() {
            anyhow::bail!(
                "gitlab: release for project '{}' (tag '{}') is missing required \
                 ref (commit SHA). GitLab POST /projects/:id/releases rejects \
                 empty `ref`. This means the git stage did not populate \
                 `ctx.git_info.commit` — re-run `task release` from inside the \
                 git working tree so git porcelain can resolve HEAD, or supply \
                 the SHA via the upstream pipeline (anodize-action ships it via \
                 `GITHUB_SHA`).",
                project_id,
                tag
            );
        }
        let create_url = format!("{}/projects/{}/releases", api, encoded);
        let payload = serde_json::json!({
            "name": name,
            "description": body,
            "ref": commit,
            "tag_name": tag,
        });

        retry_http_async(
            RetryLog::new("gitlab: POST create release", log),
            policy,
            SuccessClass::Strict,
            |_| client.post(&create_url).json(&payload).send(),
            |status, body| {
                format!(
                    "gitlab: create release failed (HTTP {status}): {}",
                    redact_bearer_tokens(body)
                )
            },
        )
        .await?;
    }

    Ok(tag.to_string())
}
