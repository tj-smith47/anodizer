//! GitLab release backend — creates releases, uploads assets, and publishes
//! releases via the GitLab REST API.
//!
//! GitLab does not support draft releases (unlike GitHub), so `PublishRelease`
//! is a no-op.  Asset uploads use either the Generic Package Registry (PUT) or
//! Project Markdown Uploads (POST multipart), then create a release link to
//! the uploaded file.
//!
//! Reference: GoReleaser `internal/client/gitlab.go`.

use std::path::Path;

use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_async};
use anodizer_core::url::percent_encode_path_segment;
use anyhow::{Context as _, Result, bail};
use reqwest::Client;

use crate::release_body::compose_body_for_mode;

// ---------------------------------------------------------------------------
// Backend ctx + per-call specs
// ---------------------------------------------------------------------------
//
// These bundle the long argument lists in `gitlab_create_release`,
// `gitlab_upload_asset`, and `upload_via_package_registry` so each function
// signature stays under clippy's 7-argument threshold without an
// `#[allow(clippy::too_many_arguments)]` suppression. The fields stay
// borrowed (`&str`/`&Path`) — these structs are short-lived call-frame
// shapes, not owned config.

/// Backend identity for a GitLab API call sequence.
///
/// Carries the HTTP client, base API URL, project_id, and retry policy — i.e.
/// everything that's constant for a whole release-publish loop. Per-release
/// fields (tag, name, body, …) live in [`GitlabReleaseSpec`]; per-asset
/// fields live in [`GitlabAssetSpec`].
#[derive(Clone, Copy)]
pub(crate) struct GitlabCtx<'a> {
    pub client: &'a Client,
    pub api_url: &'a str,
    pub project_id: &'a str,
    pub policy: &'a RetryPolicy,
}

/// Release metadata used by [`gitlab_create_release`].
#[derive(Clone, Copy)]
pub(crate) struct GitlabReleaseSpec<'a> {
    pub tag: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub commit: &'a str,
    pub release_mode: &'a str,
}

/// File-on-disk identity used by every asset-upload call.
#[derive(Clone, Copy)]
pub(crate) struct GitlabAssetSpec<'a> {
    pub file_path: &'a Path,
    pub file_name: &'a str,
}

/// Generic Package Registry coordinates — used only when the upload path
/// is the Package Registry (PUT) rather than Project Markdown Uploads.
#[derive(Clone, Copy)]
pub(crate) struct GitlabPackageRegistrySpec<'a> {
    pub project_name: &'a str,
    pub version: &'a str,
}

// ---------------------------------------------------------------------------
// URL-encoding aliases — consolidated onto `anodizer_core::url::percent_encode_path_segment`.
// GitLab, Gitea and GitHub all use the same strict segment set so a tag like
// `v1.0.0+build.1` produces identical URLs across backends.
// ---------------------------------------------------------------------------

fn encode_project_id(s: &str) -> String {
    percent_encode_path_segment(s)
}
fn encode_tag(s: &str) -> String {
    percent_encode_path_segment(s)
}
fn encode_path_segment(s: &str) -> String {
    percent_encode_path_segment(s)
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Build the GitLab project ID string from owner and name.
///
/// If `owner` is empty, only the name is returned (GitLab supports projects
/// without a namespace prefix in some API calls).
pub(crate) fn gitlab_project_id(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", owner, name)
    }
}

/// Build the release page URL on the GitLab web UI.
pub(crate) fn gitlab_release_url(download_url: &str, owner: &str, name: &str, tag: &str) -> String {
    let base = download_url.trim_end_matches('/');
    if owner.is_empty() {
        format!("{}/{}/-/releases/{}", base, name, tag)
    } else {
        format!("{}/{}/{}/-/releases/{}", base, owner, name, tag)
    }
}

/// Build the GitLab auth header name and value for the given token.
fn auth_header(use_job_token: bool) -> &'static str {
    if use_job_token {
        "JOB-TOKEN"
    } else {
        "PRIVATE-TOKEN"
    }
}

/// Resolve whether the `JOB-TOKEN` header should be used for the given token.
///
/// Mirrors GoReleaser's `checkUseJobToken` (`internal/client/gitlab.go:642`).
/// Returns true only when all three hold:
///
/// 1. `CI_JOB_TOKEN` env var is non-empty (we're inside a GitLab runner).
/// 2. `gitlab_urls.use_job_token` is true in config.
/// 3. the token being used equals `CI_JOB_TOKEN` — so secondary clients built
///    during the same CI run (e.g. Homebrew publishing with a personal token)
///    still fall back to `PRIVATE-TOKEN`.
pub(crate) fn resolve_use_job_token(config_flag: bool, token: &str) -> bool {
    let ci_token = std::env::var("CI_JOB_TOKEN").unwrap_or_default();
    if ci_token.is_empty() {
        return false;
    }
    if !config_flag {
        return false;
    }
    token == ci_token
}

/// Build a [`reqwest::Client`] configured for GitLab API access.
///
/// - `token`: the GITLAB_TOKEN or CI_JOB_TOKEN value.
/// - `skip_tls_verify`: when true, disable TLS certificate verification.
/// - `use_job_token`: when true, use `JOB-TOKEN` header instead of `PRIVATE-TOKEN`.
///
/// The token is set as a default header on all requests from the returned client.
pub(crate) fn build_gitlab_client(
    token: &str,
    skip_tls_verify: bool,
    use_job_token: bool,
) -> Result<Client> {
    let header_name = auth_header(use_job_token);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_bytes(header_name.as_bytes())
            .context("gitlab: invalid auth header name")?,
        reqwest::header::HeaderValue::from_str(token)
            .context("gitlab: invalid token value for header")?,
    );

    let builder = Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(skip_tls_verify)
        .timeout(std::time::Duration::from_secs(300));

    builder.build().context("gitlab: build HTTP client")
}

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
    } = *ctx;
    let GitlabReleaseSpec {
        tag,
        name,
        body,
        commit,
        release_mode,
    } = *spec;
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
        "gitlab: GET release by tag",
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
                "gitlab: PUT update release",
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
        // Release does not exist — create it.
        let create_url = format!("{}/projects/{}/releases", api, encoded);
        let payload = serde_json::json!({
            "name": name,
            "description": body,
            "ref": commit,
            "tag_name": tag,
        });

        retry_http_async(
            "gitlab: POST create release",
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
/// is retried — matching GoReleaser's `replace_existing_artifacts` behavior.
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
        // List existing links to find the conflicting one. This GET goes
        // through retry_http_async so transient 5xx don't lose our chance to
        // dedup the existing link.
        let list_resp = retry_http_async(
            "gitlab: GET existing release links",
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
        .await;

        match list_resp {
            Ok(list_resp) => {
                let links: Vec<serde_json::Value> = list_resp
                    .json()
                    .await
                    .context("gitlab: parse release links JSON")?;

                for link in &links {
                    if link["name"].as_str() == Some(file_name)
                        && let Some(link_id) = link["id"].as_u64()
                    {
                        let delete_url = format!("{}/{}", links_api, link_id);
                        retry_http_async(
                            "gitlab: DELETE existing release link",
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
                        break;
                    }
                }
            }
            Err(_) => {
                // Could not list links — report the original error.
                bail!(
                    "gitlab: create release link for '{}' failed (HTTP {}): {}",
                    file_name,
                    status_code,
                    redact_bearer_tokens(&text)
                );
            }
        }

        // Retry the POST after deleting the conflicting link.
        retry_http_async(
            "gitlab: POST create release link (retry after delete)",
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

/// Detect whether the GitLab server is pre-v17.
///
/// Strategy (matching GoReleaser `isV17`):
/// 1. Check `CI_SERVER_VERSION` environment variable (set in GitLab CI runners)
/// 2. Fall back to `GET /api/v4/version` API call
/// 3. If both fail, default to pre-v17 behavior (`filepath`) — conservative
///    approach matching GoReleaser, which returns `isV17 = false` on failure.
async fn detect_pre_v17_gitlab(client: &Client, api_url: &str) -> bool {
    // 1. Check environment variable first.
    if let Ok(version_str) = std::env::var("CI_SERVER_VERSION") {
        return is_pre_v17(&version_str);
    }

    // 2. Fall back to API call.
    let api = api_url.trim_end_matches('/');
    let version_url = format!("{}/version", api);
    match client.get(&version_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await
                && let Some(version_str) = body["version"].as_str()
            {
                return is_pre_v17(version_str);
            }
            // Could not parse version — default to pre-v17 (conservative).
            true
        }
        // API call failed — default to pre-v17 (conservative, matching GoReleaser).
        _ => true,
    }
}

/// Parse a GitLab version string and return true if the major version is < 17.
fn is_pre_v17(version_str: &str) -> bool {
    // CI_SERVER_VERSION is like "16.11.0" or "17.0.0"
    if let Some(major_str) = version_str.split('.').next()
        && let Ok(major) = major_str.parse::<u32>()
    {
        return major < 17;
    }
    false
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
    retry_http_async(
        "gitlab: PUT upload to package registry",
        policy,
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
async fn upload_via_project_uploads(
    client: &Client,
    api: &str,
    encoded_project_id: &str,
    file_path: &Path,
    file_name: &str,
    download_url: &str,
    policy: &RetryPolicy,
) -> Result<String> {
    let data = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("gitlab: read file {}", file_path.display()))?;

    let upload_url = format!("{}/projects/{}/uploads", api, encoded_project_id);

    // Multipart `Form` is move-only, so each retry attempt rebuilds it from
    // the cloned body bytes. `mime_str("application/octet-stream")` is
    // structurally infallible (a valid RFC-2045 token) so the error arm is
    // marked unreachable — same pattern as cloudsmith.rs::retry_request.
    let resp = retry_http_async(
        "gitlab: POST project upload",
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
    // GoReleaser constructs: `gitlabBaseURL + "/" + projectFile.FullPath`.
    // We follow the same simple approach.
    let full_path = body["full_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("gitlab: upload response missing 'full_path' field"))?;

    let base = download_url.trim_end_matches('/');
    let link = format!("{}/{}", base, full_path.trim_start_matches('/'));

    Ok(link)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- gitlab_project_id ---------------------------------------------------

    #[test]
    fn project_id_with_owner_and_name() {
        assert_eq!(
            gitlab_project_id("mygroup", "myproject"),
            "mygroup/myproject"
        );
    }

    #[test]
    fn project_id_with_empty_owner() {
        assert_eq!(gitlab_project_id("", "myproject"), "myproject");
    }

    #[test]
    fn project_id_with_nested_group() {
        assert_eq!(
            gitlab_project_id("org/subgroup", "repo"),
            "org/subgroup/repo"
        );
    }

    // -- encode_project_id ---------------------------------------------------

    #[test]
    fn encode_simple_project_id() {
        assert_eq!(
            encode_project_id("mygroup/myproject"),
            "mygroup%2Fmyproject"
        );
    }

    #[test]
    fn encode_nested_project_id() {
        assert_eq!(
            encode_project_id("org/subgroup/repo"),
            "org%2Fsubgroup%2Frepo"
        );
    }

    #[test]
    fn encode_project_id_no_slash() {
        // A project without an owner should pass through mostly unchanged.
        assert_eq!(encode_project_id("myproject"), "myproject");
    }

    // -- encode_tag ---------------------------------------------------------

    #[test]
    fn encode_tag_simple() {
        assert_eq!(encode_tag("v1.0.0"), "v1.0.0");
    }

    #[test]
    fn encode_tag_with_plus() {
        // `+` must be encoded to avoid breaking URL path segments.
        assert_eq!(encode_tag("v1.0.0+build.1"), "v1.0.0%2Bbuild.1");
    }

    #[test]
    fn encode_tag_with_special_chars() {
        // `#`, `?`, and spaces must all be encoded.
        assert_eq!(encode_tag("v1 beta#2?rc"), "v1%20beta%232%3Frc");
    }

    // -- encode_path_segment -------------------------------------------------

    #[test]
    fn encode_path_segment_simple() {
        assert_eq!(encode_path_segment("myproject"), "myproject");
    }

    #[test]
    fn encode_path_segment_with_slash() {
        assert_eq!(encode_path_segment("my/project"), "my%2Fproject");
    }

    #[test]
    fn encode_path_segment_preserves_dots_and_dashes() {
        assert_eq!(encode_path_segment("my-project.v2"), "my-project.v2");
    }

    // -- is_pre_v17 (version parsing) ------------------------------------------

    #[test]
    fn is_pre_v17_with_v16() {
        assert!(is_pre_v17("16.11.0"));
    }

    #[test]
    fn is_pre_v17_with_v15() {
        assert!(is_pre_v17("15.0.0"));
    }

    #[test]
    fn is_pre_v17_with_v17() {
        assert!(!is_pre_v17("17.0.0"));
    }

    #[test]
    fn is_pre_v17_with_v18() {
        assert!(!is_pre_v17("18.1.2"));
    }

    #[test]
    fn is_pre_v17_with_empty() {
        assert!(!is_pre_v17(""));
    }

    #[test]
    fn is_pre_v17_with_garbage() {
        assert!(!is_pre_v17("not-a-version"));
    }

    // -- gitlab_release_url --------------------------------------------------

    #[test]
    fn release_url_with_owner() {
        let url = gitlab_release_url("https://gitlab.com", "mygroup", "myproject", "v1.0.0");
        assert_eq!(
            url,
            "https://gitlab.com/mygroup/myproject/-/releases/v1.0.0"
        );
    }

    #[test]
    fn release_url_without_owner() {
        let url = gitlab_release_url("https://gitlab.com", "", "myproject", "v1.0.0");
        assert_eq!(url, "https://gitlab.com/myproject/-/releases/v1.0.0");
    }

    #[test]
    fn release_url_trailing_slash_stripped() {
        let url = gitlab_release_url("https://gitlab.example.com/", "org", "repo", "v2.0.0");
        assert_eq!(url, "https://gitlab.example.com/org/repo/-/releases/v2.0.0");
    }

    // -- build_gitlab_client -------------------------------------------------

    #[test]
    fn build_client_with_private_token() {
        let client = build_gitlab_client("glpat-xxxx", false, false);
        assert!(client.is_ok());
    }

    #[test]
    fn build_client_with_job_token() {
        let client = build_gitlab_client("job-token-value", false, true);
        assert!(client.is_ok());
    }

    #[test]
    fn build_client_with_skip_tls() {
        let client = build_gitlab_client("glpat-xxxx", true, false);
        assert!(client.is_ok());
    }

    #[test]
    fn build_client_with_all_options() {
        let client = build_gitlab_client("job-token", true, true);
        assert!(client.is_ok());
    }

    // -- auth_header ---------------------------------------------------------

    #[test]
    fn auth_header_private_token() {
        assert_eq!(auth_header(false), "PRIVATE-TOKEN");
    }

    #[test]
    fn auth_header_job_token() {
        assert_eq!(auth_header(true), "JOB-TOKEN");
    }

    // -- resolve_use_job_token -----------------------------------------------
    // Mutates CI_JOB_TOKEN env var — must run serially to avoid races with
    // other tests in the workspace that may read the same variable.

    #[test]
    #[serial_test::serial]
    fn resolve_use_job_token_in_ci_flag_on_tokens_match() {
        unsafe { std::env::set_var("CI_JOB_TOKEN", "real-ci-token") };
        assert!(resolve_use_job_token(true, "real-ci-token"));
        unsafe { std::env::remove_var("CI_JOB_TOKEN") };
    }

    #[test]
    #[serial_test::serial]
    fn resolve_use_job_token_in_ci_flag_on_tokens_differ() {
        unsafe { std::env::set_var("CI_JOB_TOKEN", "real-ci-token") };
        assert!(!resolve_use_job_token(true, "glpat-xyz"));
        unsafe { std::env::remove_var("CI_JOB_TOKEN") };
    }

    #[test]
    #[serial_test::serial]
    fn resolve_use_job_token_in_ci_flag_off() {
        unsafe { std::env::set_var("CI_JOB_TOKEN", "real-ci-token") };
        assert!(!resolve_use_job_token(false, "real-ci-token"));
        unsafe { std::env::remove_var("CI_JOB_TOKEN") };
    }

    #[test]
    #[serial_test::serial]
    fn resolve_use_job_token_no_ci_env() {
        unsafe { std::env::remove_var("CI_JOB_TOKEN") };
        assert!(!resolve_use_job_token(true, "glpat-xyz"));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_use_job_token_empty_ci_env() {
        unsafe { std::env::set_var("CI_JOB_TOKEN", "") };
        assert!(!resolve_use_job_token(true, ""));
        unsafe { std::env::remove_var("CI_JOB_TOKEN") };
    }

    // -- gitlab_create_release retry behaviour (P1.4) ------------------------
    //
    // Pin: a 503 on the GET-release-by-tag probe must be retried (transient
    // GitLab 5xx), not fast-failed. Mirror the equivalent core::retry test
    // (`retry_http_async_retries_5xx_then_succeeds`) but at the publisher
    // layer so the caller-supplied policy reaches the helper.

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    #[tokio::test]
    async fn gitlab_create_release_retries_5xx_on_get_probe() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        // Sequence: 503 on the GET probe, then 200 with an empty release JSON
        // (release exists), then 200 on the PUT update. The retry helper
        // should swallow the 503 and proceed.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"description\":\"old\"}\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
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

        let ctx = GitlabCtx {
            client: &client,
            api_url: &api_url,
            project_id: "myorg/myproj",
            policy: &policy,
        };
        let spec = GitlabReleaseSpec {
            tag: "v1.0.0",
            name: "Release v1.0.0",
            body: "new body",
            commit: "abc123",
            release_mode: "replace",
        };
        let result = gitlab_create_release(&ctx, &spec).await;

        assert!(
            result.is_ok(),
            "expected success after 5xx retry, got: {:?}",
            result.err().map(|e| format!("{e:#}"))
        );
        // Three connections total: one retried GET (1 503 + 1 200 = 2) plus
        // one PUT = 3.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 3 connections (503-retry GET, 200 GET, 200 PUT)"
        );
    }

    /// Defense-in-depth: a GitLab API 4xx response that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. Exercises the
    /// `gitlab_create_release` GET-probe error-message closure on the
    /// 401-fast-fail path. Other gitlab.rs body-interpolation sites share
    /// the same redaction wrap.
    #[tokio::test]
    async fn gitlab_create_release_redacts_bearer_in_error_body() {
        use std::time::Duration;

        let leaky = r#"{"message":"401 Unauthorized: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
        let body_len = leaky.len();
        // 401 fast-fails (not 403/404 which are the "release missing" signal).
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
        let ctx = GitlabCtx {
            client: &client,
            api_url: &api_url,
            project_id: "myorg/myproj",
            policy: &policy,
        };
        let spec = GitlabReleaseSpec {
            tag: "v1.0.0",
            name: "Release v1.0.0",
            body: "new body",
            commit: "abc123",
            release_mode: "replace",
        };
        let err = gitlab_create_release(&ctx, &spec)
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
}
