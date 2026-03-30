use anyhow::Result;
use serde_json::json;

const GRAPHQL_URL: &str = "https://api.opencollective.com/graphql/v2";

pub const DEFAULT_TITLE_TEMPLATE: &str = "{{ .Tag }}";
pub const DEFAULT_MESSAGE_TEMPLATE: &str =
    r#"{{ .ProjectName }} {{ .Tag }} is out!<br/>Check it out at <a href="{{ .ReleaseURL }}">{{ .ReleaseURL }}</a>"#;

pub fn send_opencollective(token: &str, slug: &str, title: &str, html: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();

    // Step 1: Create update
    let create_query = r#"mutation($update: UpdateCreateInput!) { createUpdate(update: $update) { id } }"#;
    let create_body = json!({
        "query": create_query,
        "variables": {
            "update": {
                "title": title,
                "html": html,
                "account": { "slug": slug }
            }
        }
    });

    let resp = client
        .post(GRAPHQL_URL)
        .header("Personal-Token", token)
        .header("Content-Type", "application/json")
        .body(create_body.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("opencollective: createUpdate failed ({status}): {body}");
    }

    let resp_text = resp.text()?;
    let resp_json: serde_json::Value = serde_json::from_str(&resp_text)?;
    let update_id = resp_json["data"]["createUpdate"]["id"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!("opencollective: missing update ID in createUpdate response")
        })?;

    // Step 2: Publish update
    let publish_query = r#"mutation($id: String!, $audience: UpdateAudience) { publishUpdate(id: $id, notificationAudience: $audience) { id } }"#;
    let publish_body = json!({
        "query": publish_query,
        "variables": {"id": update_id, "audience": "ALL"}
    });

    let resp = client
        .post(GRAPHQL_URL)
        .header("Personal-Token", token)
        .header("Content-Type", "application/json")
        .body(publish_body.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("opencollective: publishUpdate failed ({status}): {body}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graphql_url_constant() {
        assert_eq!(GRAPHQL_URL, "https://api.opencollective.com/graphql/v2");
    }

    #[test]
    fn test_default_title_template() {
        assert_eq!(DEFAULT_TITLE_TEMPLATE, "{{ .Tag }}");
    }

    #[test]
    fn test_default_message_template_contains_html() {
        assert!(DEFAULT_MESSAGE_TEMPLATE.contains("<br/>"));
        assert!(DEFAULT_MESSAGE_TEMPLATE.contains("<a href="));
        assert!(DEFAULT_MESSAGE_TEMPLATE.contains("{{ .ProjectName }}"));
        assert!(DEFAULT_MESSAGE_TEMPLATE.contains("{{ .Tag }}"));
        assert!(DEFAULT_MESSAGE_TEMPLATE.contains("{{ .ReleaseURL }}"));
    }
}
