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
        .header("User-Agent", "anodize/1.0")
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
    use super::*;

    #[test]
    fn test_post_payload_structure() {
        let body = json!({
            "title": "myapp v1.0 is out!",
            "raw": "Check it out",
            "category": 5,
        });
        assert_eq!(body["title"], "myapp v1.0 is out!");
        assert_eq!(body["raw"], "Check it out");
        assert_eq!(body["category"], 5);
    }
}
