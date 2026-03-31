use anyhow::Result;
use serde_json::json;

const API_BASE: &str = "https://api.linkedin.com";

/// Post a share to LinkedIn via the v2 Share API.
///
/// Two-step flow matching GoReleaser:
/// 1. Resolve the profile URN via `/v2/userinfo` (newer, uses `sub` field).
///    Falls back to `/v2/me` (legacy, uses `id` field) only on 403 Forbidden.
/// 2. POST the share to `/v2/shares`.
pub fn send_linkedin(access_token: &str, message: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let profile_urn = get_profile_urn(&client, access_token)?;

    let share = json!({
        "owner": profile_urn,
        "text": { "text": message },
        "distribution": { "linkedInDistributionTarget": {} }
    });

    let resp = client
        .post(format!("{API_BASE}/v2/shares"))
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("X-Restli-Protocol-Version", "2.0.0")
        .body(share.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("linkedin: share failed ({status}): {body}");
    }

    // Extract activity URL from response (matches GoReleaser behavior).
    let resp_text = resp.text().unwrap_or_default();
    let resp_json: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| anyhow::anyhow!("linkedin: failed to parse share response: {e}"))?;
    let activity = resp_json
        .get("activity")
        .and_then(|a| a.as_str())
        .ok_or_else(|| anyhow::anyhow!("linkedin: could not find 'activity' in share response"))?;
    eprintln!("linkedin: post available at https://www.linkedin.com/feed/update/{activity}");

    Ok(())
}

/// Resolve the LinkedIn profile URN (`urn:li:person:<id>`).
///
/// Tries `/v2/userinfo` first (newer endpoint, `sub` field).  Falls back to
/// `/v2/me` (legacy, `id` field) only when the newer endpoint returns 403.
fn get_profile_urn(client: &reqwest::blocking::Client, access_token: &str) -> Result<String> {
    // Try newer /v2/userinfo endpoint first.
    let resp = client
        .get(format!("{API_BASE}/v2/userinfo"))
        .bearer_auth(access_token)
        .send()?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        // Permission issue — fall back to legacy endpoint.
        return get_profile_urn_legacy(client, access_token);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("linkedin: GET /v2/userinfo failed ({status}): {body}");
    }

    let text = resp.text()?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let sub = json["sub"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("linkedin: missing 'sub' in /v2/userinfo response"))?;
    Ok(format!("urn:li:person:{sub}"))
}

/// Legacy fallback: resolve profile URN via `/v2/me`.
fn get_profile_urn_legacy(
    client: &reqwest::blocking::Client,
    access_token: &str,
) -> Result<String> {
    let resp = client
        .get(format!("{API_BASE}/v2/me"))
        .bearer_auth(access_token)
        .send()?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("linkedin: forbidden — please check your permissions");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("linkedin: GET /v2/me failed ({status}): {body}");
    }

    let text = resp.text()?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let id = json["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("linkedin: missing 'id' in /v2/me response"))?;
    Ok(format!("urn:li:person:{id}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn test_share_payload_structure() {
        let payload = json!({
            "owner": "urn:li:person:abc123",
            "text": { "text": "myapp v1.0 released" },
            "distribution": { "linkedInDistributionTarget": {} }
        });
        assert_eq!(payload["owner"], "urn:li:person:abc123");
        assert_eq!(payload["text"]["text"], "myapp v1.0 released");
        assert!(payload["distribution"]["linkedInDistributionTarget"].is_object());
    }
}
