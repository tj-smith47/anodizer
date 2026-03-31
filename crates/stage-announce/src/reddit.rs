use std::collections::HashMap;

use anyhow::Result;

/// Authenticate with Reddit's OAuth2 API and submit a link post to a subreddit.
///
/// 1. POST to `/api/v1/access_token` with Basic Auth (application_id:secret)
///    and `grant_type=password` to obtain a bearer token.
/// 2. POST to `/api/submit` on `oauth.reddit.com` with the bearer token to
///    create the link post.
pub fn send_reddit(
    application_id: &str,
    secret: &str,
    username: &str,
    password: &str,
    subreddit: &str,
    title: &str,
    url: &str,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("anodize/1.0")
        .build()?;

    // Step 1: Get OAuth token
    let token_resp = client
        .post("https://www.reddit.com/api/v1/access_token")
        .basic_auth(application_id, Some(secret))
        .form(&[
            ("grant_type", "password"),
            ("username", username),
            ("password", password),
        ])
        .send()?;

    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let body = token_resp.text().unwrap_or_default();
        anyhow::bail!("reddit: OAuth token request failed ({status}): {body}");
    }

    let token_body = token_resp.text()?;
    let token_json: serde_json::Value = serde_json::from_str(&token_body)?;
    let access_token = token_json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("reddit: missing access_token in OAuth response"))?;

    // Step 2: Submit link
    let mut form = HashMap::new();
    form.insert("api_type", "json");
    form.insert("kind", "link");
    form.insert("sr", subreddit);
    form.insert("title", title);
    form.insert("url", url);

    let submit_resp = client
        .post("https://oauth.reddit.com/api/submit")
        .bearer_auth(access_token)
        .form(&form)
        .send()?;

    if !submit_resp.status().is_success() {
        let status = submit_resp.status();
        let body = submit_resp.text().unwrap_or_default();
        anyhow::bail!("reddit: submit failed ({status}): {body}");
    }

    // Reddit returns 200 even on failure — check json.errors
    let submit_body: serde_json::Value = serde_json::from_str(&submit_resp.text()?)?;
    if let Some(errors) = submit_body
        .get("json")
        .and_then(|j| j.get("errors"))
        .and_then(|e| e.as_array())
        && !errors.is_empty()
    {
        anyhow::bail!("reddit: submit returned errors: {errors:?}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {}
