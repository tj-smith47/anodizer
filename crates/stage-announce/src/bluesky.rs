use anodizer_core::retry::RetryPolicy;
use anyhow::{Context as _, Result};
use serde_json::json;

use crate::helpers::retry_http;

/// Default Bluesky PDS (Personal Data Server). Override via
/// `bluesky.pds_url` in config to target a self-hosted PDS.
pub const DEFAULT_PDS_URL: &str = "https://bsky.social";

pub fn send_bluesky(
    username: &str,
    app_password: &str,
    message: &str,
    release_url: Option<&str>,
    pds_url: Option<&str>,
    policy: &RetryPolicy,
) -> Result<()> {
    let pds_url = pds_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_PDS_URL.to_string());
    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .build()
        .context("bluesky: build HTTP client")?;

    let session_payload = json!({
        "identifier": username,
        "password": app_password,
    })
    .to_string();
    let session_text = retry_http("bluesky", "createSession", policy, || {
        client
            .post(format!("{pds_url}/xrpc/com.atproto.server.createSession"))
            .header("Content-Type", "application/json")
            .body(session_payload.clone())
            .send()
    })?;
    let session: serde_json::Value = serde_json::from_str(&session_text)
        .context("bluesky: createSession response was not valid JSON")?;
    let access_jwt = session["accessJwt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing accessJwt in session response"))?;
    let did = session["did"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing did in session response"))?;

    let now = anodizer_core::sde::resolve_now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut record = json!({
        "$type": "app.bsky.feed.post",
        "text": message,
        "createdAt": now,
    });

    // Add link facet if release_url is found in message
    if let Some(url) = release_url
        && let Some(byte_start) = message.find(url)
    {
        let byte_end = byte_start + url.len();
        record["facets"] = json!([{
            "index": {"byteStart": byte_start, "byteEnd": byte_end},
            "features": [{"$type": "app.bsky.richtext.facet#link", "uri": url}]
        }]);
    }

    let create_body = json!({
        "repo": did,
        "collection": "app.bsky.feed.post",
        "record": record,
    })
    .to_string();

    let _ = retry_http("bluesky", "createRecord", policy, || {
        client
            .post(format!("{pds_url}/xrpc/com.atproto.repo.createRecord"))
            .bearer_auth(access_jwt)
            .header("Content-Type", "application/json")
            .body(create_body.clone())
            .send()
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_link_facet_detection() {
        let message =
            "myapp v1.0.0 is out! Check it out at https://github.com/org/repo/releases/tag/v1.0.0";
        let url = "https://github.com/org/repo/releases/tag/v1.0.0";
        let byte_start = message.find(url).unwrap();
        let byte_end = byte_start + url.len();
        assert_eq!(byte_start, 37);
        assert_eq!(byte_end, 37 + url.len());
    }

    #[test]
    fn test_link_facet_not_found() {
        let message = "myapp v1.0.0 is out!";
        let url = "https://github.com/org/repo/releases/tag/v1.0.0";
        assert!(message.find(url).is_none());
    }
}
