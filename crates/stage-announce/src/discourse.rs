use anyhow::Result;
use serde_json::json;

/// Create a new topic on a Discourse forum.
///
/// Posts to `{server}/posts.json` with API key authentication.
/// The topic is created in the specified category with the given title and message.
pub fn send_discourse(
    server: &str,
    api_key: &str,
    username: &str,
    category_id: u64,
    title: &str,
    message: &str,
) -> Result<()> {
    let url = format!("{}/posts.json", server.trim_end_matches('/'));
    let body = json!({
        "title": title,
        "raw": message,
        "category": category_id,
    });

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("Api-Key", api_key)
        .header("Api-Username", username)
        .header("Content-Type", "application/json")
        .header("User-Agent", anodize_core::http::USER_AGENT)
        .body(body.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("discourse: failed to create topic ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_url_construction_strips_trailing_slash() {
        let server = "https://forum.example.com/";
        let url = format!("{}/posts.json", server.trim_end_matches('/'));
        assert_eq!(url, "https://forum.example.com/posts.json");
    }

    #[test]
    fn test_url_construction_no_trailing_slash() {
        let server = "https://forum.example.com";
        let url = format!("{}/posts.json", server.trim_end_matches('/'));
        assert_eq!(url, "https://forum.example.com/posts.json");
    }
}
