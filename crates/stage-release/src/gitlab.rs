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

use anyhow::{Context as _, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Client;

use crate::compose_body_for_mode;

// ---------------------------------------------------------------------------
// URL-encoding helpers
// ---------------------------------------------------------------------------

/// Characters that must be percent-encoded in a GitLab project path segment.
/// GitLab requires the full project path (e.g. `group/project`) to be encoded
/// so that `/` becomes `%2F`.
const PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.');

/// Characters safe in a single URL path segment (no `/`).
/// Used for tag names, package names, versions, and file names in URLs.
const SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.');

/// Percent-encode a GitLab project ID path segment.
///
/// `owner/name` becomes `owner%2Fname`.
fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PATH_ENCODE_SET).to_string()
}

/// Percent-encode a tag for use in URL path segments.
///
/// Tags may contain `+`, `#`, `?`, spaces, or other characters that break URLs.
/// e.g. `v1.0.0+build.1` becomes `v1.0.0%2Bbuild.1`.
fn encode_tag(tag: &str) -> String {
    utf8_percent_encode(tag, SEGMENT_ENCODE_SET).to_string()
}

/// Percent-encode a single URL path component (package name, version, filename).
fn encode_path_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT_ENCODE_SET).to_string()
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
pub(crate) fn gitlab_release_url(
    download_url: &str,
    owner: &str,
    name: &str,
    tag: &str,
) -> String {
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
/// Returns the tag name (GitLab's release identifier).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn gitlab_create_release(
    client: &Client,
    api_url: &str,
    project_id: &str,
    tag: &str,
    name: &str,
    body: &str,
    commit: &str,
    release_mode: &str,
) -> Result<String> {
    let api = api_url.trim_end_matches('/');
    let encoded = encode_project_id(project_id);
    let encoded_tag = encode_tag(tag);

    // Try to get the existing release for this tag.
    let get_url = format!("{}/projects/{}/releases/{}", api, encoded, encoded_tag);
    let get_resp = client.get(&get_url).send().await.context(
        "gitlab: GET release by tag",
    )?;

    let status = get_resp.status().as_u16();

    if status == 403 || status == 404 {
        // Release does not exist — create it.
        let create_url = format!("{}/projects/{}/releases", api, encoded);
        let payload = serde_json::json!({
            "name": name,
            "description": body,
            "ref": commit,
            "tag_name": tag,
        });

        let resp = client
            .post(&create_url)
            .json(&payload)
            .send()
            .await
            .context("gitlab: POST create release")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "gitlab: create release failed (HTTP {}): {}",
                status,
                text
            );
        }
    } else if get_resp.status().is_success() {
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

        let resp = client
            .put(&update_url)
            .json(&payload)
            .send()
            .await
            .context("gitlab: PUT update release")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "gitlab: update release failed (HTTP {}): {}",
                status,
                text
            );
        }
    } else {
        // Unexpected error.
        let text = get_resp.text().await.unwrap_or_default();
        bail!(
            "gitlab: check existing release failed (HTTP {}): {}",
            status,
            text
        );
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn gitlab_upload_asset(
    client: &Client,
    api_url: &str,
    project_id: &str,
    tag: &str,
    file_path: &Path,
    file_name: &str,
    project_name: &str,
    version: &str,
    use_package_registry: bool,
    download_url: &str,
    replace_existing: bool,
) -> Result<()> {
    let api = api_url.trim_end_matches('/');
    let encoded = encode_project_id(project_id);
    let encoded_tag = encode_tag(tag);

    let link_url = if use_package_registry {
        upload_via_package_registry(client, api, &encoded, project_name, version, file_name, file_path)
            .await?
    } else {
        upload_via_project_uploads(client, api, &encoded, file_path, file_name, download_url)
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
    let use_legacy_file_path = detect_pre_v17_gitlab();
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
        let text = resp.text().await.unwrap_or_default();
        // List existing links to find the conflicting one.
        let list_resp = client
            .get(&links_api)
            .send()
            .await
            .context("gitlab: GET existing release links for replace")?;

        if list_resp.status().is_success() {
            let links: Vec<serde_json::Value> = list_resp
                .json()
                .await
                .context("gitlab: parse release links JSON")?;

            for link in &links {
                if link["name"].as_str() == Some(file_name) {
                    if let Some(link_id) = link["id"].as_u64() {
                        let delete_url =
                            format!("{}/{}", links_api, link_id);
                        let del_resp = client
                            .delete(&delete_url)
                            .send()
                            .await
                            .with_context(|| {
                                format!(
                                    "gitlab: DELETE existing release link '{}' (id={})",
                                    file_name, link_id
                                )
                            })?;
                        if !del_resp.status().is_success() {
                            bail!(
                                "gitlab: delete existing link '{}' failed (HTTP {}): {}",
                                file_name,
                                del_resp.status(),
                                del_resp.text().await.unwrap_or_default()
                            );
                        }
                        break;
                    }
                }
            }
        } else {
            // Could not list links — report the original error.
            bail!(
                "gitlab: create release link for '{}' failed (HTTP {}): {}",
                file_name,
                status_code,
                text
            );
        }

        // Retry the POST after deleting the conflicting link.
        let retry_resp = client
            .post(&links_api)
            .json(&payload)
            .send()
            .await
            .context("gitlab: POST create release link (retry after delete)")?;

        if !retry_resp.status().is_success() {
            let retry_status = retry_resp.status();
            let retry_text = retry_resp.text().await.unwrap_or_default();
            bail!(
                "gitlab: create release link for '{}' failed on retry (HTTP {}): {}",
                file_name,
                retry_status,
                retry_text
            );
        }
    } else {
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "gitlab: create release link for '{}' failed (HTTP {}): {}",
            file_name,
            status_code,
            text
        );
    }

    Ok(())
}

/// Detect whether the GitLab server is pre-v17 by checking the
/// `CI_SERVER_VERSION` environment variable (set in GitLab CI runners).
///
/// If the env var is absent or unparseable, defaults to v17+ behavior
/// (using `direct_asset_path`).
fn detect_pre_v17_gitlab() -> bool {
    if let Ok(version_str) = std::env::var("CI_SERVER_VERSION") {
        return is_pre_v17(&version_str);
    }
    false
}

/// Parse a GitLab version string and return true if the major version is < 17.
fn is_pre_v17(version_str: &str) -> bool {
    // CI_SERVER_VERSION is like "16.11.0" or "17.0.0"
    if let Some(major_str) = version_str.split('.').next() {
        if let Ok(major) = major_str.parse::<u32>() {
            return major < 17;
        }
    }
    false
}

/// Upload a file via the GitLab Generic Package Registry.
///
/// ```text
/// PUT {api}/projects/{id}/packages/generic/{package}/{version}/{filename}
/// ```
async fn upload_via_package_registry(
    client: &Client,
    api: &str,
    encoded_project_id: &str,
    project_name: &str,
    version: &str,
    file_name: &str,
    file_path: &Path,
) -> Result<String> {
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

    let resp = client
        .put(&upload_url)
        .header("Content-Type", "application/octet-stream")
        .body(data)
        .send()
        .await
        .with_context(|| format!("gitlab: PUT upload '{}' to package registry", file_name))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "gitlab: package registry upload '{}' failed (HTTP {}): {}",
            file_name,
            status,
            text
        );
    }

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
) -> Result<String> {
    let data = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("gitlab: read file {}", file_path.display()))?;

    let upload_url = format!("{}/projects/{}/uploads", api, encoded_project_id);

    let file_part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name.to_string())
        .mime_str("application/octet-stream")
        .context("gitlab: set MIME type for upload")?;

    let form = reqwest::multipart::Form::new().part("file", file_part);

    let resp = client
        .post(&upload_url)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("gitlab: POST upload '{}' as project attachment", file_name))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "gitlab: project upload '{}' failed (HTTP {}): {}",
            file_name,
            status,
            text
        );
    }

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
        assert_eq!(gitlab_project_id("mygroup", "myproject"), "mygroup/myproject");
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
        assert_eq!(encode_project_id("mygroup/myproject"), "mygroup%2Fmyproject");
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
        assert_eq!(
            url,
            "https://gitlab.example.com/org/repo/-/releases/v2.0.0"
        );
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
}
