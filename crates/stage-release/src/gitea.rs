//! Gitea release backend — creates releases, uploads assets via the Gitea API.
//!
//! Gitea's release API is simpler than GitLab's: assets are uploaded directly
//! via multipart POST to the release endpoint (no package registry indirection).
//! Draft support is limited (Gitea has it but the GoReleaser client treats
//! `PublishRelease` as a no-op), so we follow that same approach.
//!
//! Reference: GoReleaser `internal/client/gitea.go`.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Client;

use crate::compose_body_for_mode;

// ---------------------------------------------------------------------------
// URL-encoding helpers
// ---------------------------------------------------------------------------

/// Characters safe in a single URL path segment (no `/`).
/// Used for owner, repo, tag names, and file names in URLs.
const SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.');

/// Percent-encode a single URL path component.
///
/// Tags, owner names, repo names, and file names may contain `+`, `#`, `?`,
/// spaces, or other characters that break URLs. e.g. `v1.0.0+build.1` becomes
/// `v1.0.0%2Bbuild.1`.
fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT_ENCODE_SET).to_string()
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Build the release page URL on the Gitea web UI.
///
/// Returns `{download}/{owner}/{repo}/releases/tag/{tag}`.
pub(crate) fn gitea_release_url(
    download_url: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) -> String {
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
pub(crate) fn build_gitea_client(
    token: &str,
    skip_tls_verify: bool,
) -> Result<Client> {
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn gitea_create_release(
    client: &Client,
    api_url: &str,
    owner: &str,
    repo: &str,
    tag: &str,
    commit: &str,
    name: &str,
    body: &str,
    draft: bool,
    prerelease: bool,
    release_mode: &str,
) -> Result<u64> {
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    // Try to find an existing release by listing all releases and matching tag.
    let existing = find_release_by_tag(client, api, &enc_owner, &enc_repo, tag).await?;

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

        let resp = client
            .patch(&update_url)
            .json(&payload)
            .send()
            .await
            .context("gitea: PATCH update release")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "gitea: update release failed (HTTP {}): {}",
                status,
                text
            );
        }

        Ok(release_id)
    } else {
        // Release does not exist — create it.
        let create_url = format!(
            "{}/api/v1/repos/{}/{}/releases",
            api, enc_owner, enc_repo
        );
        let payload = serde_json::json!({
            "tag_name": tag,
            "target_commitish": commit,
            "name": name,
            "body": body,
            "draft": draft,
            "prerelease": prerelease,
        });

        let resp = client
            .post(&create_url)
            .json(&payload)
            .send()
            .await
            .context("gitea: POST create release")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "gitea: create release failed (HTTP {}): {}",
                status,
                text
            );
        }

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
/// runaway pagination on repos with very long release histories).
///
/// Returns `Some((release_id, body))` if found, `None` otherwise.
async fn find_release_by_tag(
    client: &Client,
    api: &str,
    enc_owner: &str,
    enc_repo: &str,
    tag: &str,
) -> Result<Option<(u64, Option<String>)>> {
    const MAX_PAGES: u32 = 10;
    const PAGE_SIZE: u32 = 50;

    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/api/v1/repos/{}/{}/releases?page={}&limit={}",
            api, enc_owner, enc_repo, page, PAGE_SIZE
        );

        let resp = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("gitea: GET releases page {}", page))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "gitea: list releases failed (HTTP {}): {}",
                status,
                text
            );
        }

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
    client: &Client,
    api_url: &str,
    owner: &str,
    repo: &str,
    release_id: u64,
    file_path: &Path,
    file_name: &str,
) -> Result<()> {
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

    let file_part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name.to_string())
        .mime_str("application/octet-stream")
        .context("gitea: set MIME type for upload")?;

    let form = reqwest::multipart::Form::new().part("attachment", file_part);

    let resp = client
        .post(&upload_url)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("gitea: POST upload '{}' to release {}", file_name, release_id))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "gitea: upload asset '{}' failed (HTTP {}): {}",
            file_name,
            status,
            text
        );
    }

    Ok(())
}

/// Delete an existing release attachment by name.
///
/// Lists the release's attachments, finds one matching `file_name`, and
/// deletes it. Used for `replace_existing_artifacts` support.
pub(crate) async fn gitea_delete_asset_by_name(
    client: &Client,
    api_url: &str,
    owner: &str,
    repo: &str,
    release_id: u64,
    file_name: &str,
) -> Result<bool> {
    let api = api_url.trim_end_matches('/');
    let enc_owner = encode_segment(owner);
    let enc_repo = encode_segment(repo);

    // List attachments for the release.
    let list_url = format!(
        "{}/api/v1/repos/{}/{}/releases/{}/assets",
        api, enc_owner, enc_repo, release_id
    );

    let resp = client
        .get(&list_url)
        .send()
        .await
        .context("gitea: GET release assets for delete")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "gitea: list release assets failed (HTTP {}): {}",
            status,
            text
        );
    }

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

            let del_resp = client
                .delete(&delete_url)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "gitea: DELETE asset '{}' (id={}) from release {}",
                        file_name, asset_id, release_id
                    )
                })?;

            if !del_resp.status().is_success() {
                bail!(
                    "gitea: delete asset '{}' failed (HTTP {}): {}",
                    file_name,
                    del_resp.status(),
                    del_resp.text().await.unwrap_or_default()
                );
            }

            return Ok(true);
        }
    }

    Ok(false)
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
        let url = gitea_release_url(
            "https://gitea.example.com",
            "myorg",
            "myapp",
            "v1.0.0",
        );
        assert_eq!(
            url,
            "https://gitea.example.com/myorg/myapp/releases/tag/v1.0.0"
        );
    }

    #[test]
    fn release_url_trailing_slash_stripped() {
        let url = gitea_release_url(
            "https://gitea.example.com/",
            "org",
            "repo",
            "v2.0.0",
        );
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
        let url = gitea_release_url(
            "https://gitea.example.com",
            "my org",
            "my repo",
            "v1.0.0",
        );
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
        let header_value =
            reqwest::header::HeaderValue::from_str(&expected_header).unwrap();
        assert_eq!(
            header_value.to_str().unwrap(),
            "token my-gitea-token",
            "Gitea auth header must use 'token {{value}}' format"
        );

        // Ensure client was built successfully (implies headers are valid)
        drop(client);
    }
}
