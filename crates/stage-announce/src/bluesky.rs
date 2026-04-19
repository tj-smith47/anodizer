use anyhow::Result;
use serde_json::json;

/// Default Bluesky PDS (Personal Data Server). Override via
/// `bluesky.pds_url` in config to target a self-hosted PDS.
pub const DEFAULT_PDS_URL: &str = "https://bsky.social";

pub fn send_bluesky(
    username: &str,
    app_password: &str,
    message: &str,
    release_url: Option<&str>,
    pds_url: Option<&str>,
) -> Result<()> {
    let pds_url = pds_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_PDS_URL.to_string());
    let client = reqwest::blocking::Client::builder()
        .user_agent(anodize_core::http::USER_AGENT)
        .build()?;

    // Step 1: Create session (login)
    let session_resp = client
        .post(format!("{pds_url}/xrpc/com.atproto.server.createSession"))
        .header("Content-Type", "application/json")
        .body(
            json!({
                "identifier": username,
                "password": app_password,
            })
            .to_string(),
        )
        .send()?;

    if !session_resp.status().is_success() {
        let status = session_resp.status();
        let body = session_resp.text().unwrap_or_default();
        anyhow::bail!("bluesky: login failed ({status}): {body}");
    }

    let session_text = session_resp.text()?;
    let session: serde_json::Value = serde_json::from_str(&session_text)?;
    let access_jwt = session["accessJwt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing accessJwt in session response"))?;
    let did = session["did"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing did in session response"))?;

    // Step 2: Build post record
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
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
    });

    let create_resp = client
        .post(format!("{pds_url}/xrpc/com.atproto.repo.createRecord"))
        .bearer_auth(access_jwt)
        .header("Content-Type", "application/json")
        .body(create_body.to_string())
        .send()?;

    if !create_resp.status().is_success() {
        let status = create_resp.status();
        let body = create_resp.text().unwrap_or_default();
        anyhow::bail!("bluesky: post creation failed ({status}): {body}");
    }

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
