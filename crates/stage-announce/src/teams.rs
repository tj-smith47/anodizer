use anodizer_core::retry::RetryPolicy;
use anyhow::Result;
use serde_json::json;

use crate::http::post_json;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Optional fields for Microsoft Teams Adaptive Card payloads.
pub struct TeamsOptions<'a> {
    pub title: Option<&'a str>,
    pub color: Option<&'a str>,
    pub icon_url: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Webhook URL classification
// ---------------------------------------------------------------------------

/// Microsoft Teams webhook URL category.
///
/// Microsoft has announced deprecation of MessageCard webhooks served by
/// `*.webhook.office.com` (and the older `outlook.office.com/webhook/...`
/// form) at the end of 2026; new integrations should use Power Automate
/// Workflow URLs (`logic.azure.com` / `azure-api.net`). Anodizer accepts
/// **both** shapes — legacy URLs still POST successfully, but the send
/// helper emits a `tracing::warn!` referencing Microsoft's deprecation
/// note so a user importing a GoReleaser config knows the path is on a
/// migration deadline.
///
/// See the Microsoft connectors-overview page for the deprecation
/// timeline:
/// <https://learn.microsoft.com/en-us/microsoftteams/platform/m365-apps/connectors/connectors-overview>
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TeamsWebhookKind {
    /// Legacy MessageCard-style webhook (`*.webhook.office.com` or
    /// `outlook.office.com/webhook/...`). Forward to it, but warn.
    Legacy,
    /// Power Automate Workflow URL (Adaptive Card native). Silent path.
    Workflow,
}

/// Classify a Microsoft Teams webhook URL as Legacy (MessageCard
/// connector, deprecated end of 2026) or Workflow (Power Automate, the
/// forward-correct shape).
///
/// Legacy patterns recognized:
/// - host matches `*.webhook.office.com` (any subdomain, including
///   `prod-NN.webhook.office.com` and tenant-prefixed forms)
/// - host is `outlook.office.com` AND path begins with `/webhook/`
///
/// Everything else (Power Automate `*.logic.azure.com`, APIM
/// `*.azure-api.net`, corporate proxies) defaults to `Workflow`. We do
/// not strictly *require* a Workflow URL to match a known host — that
/// would block users who tunnel their webhook through a gateway and lock
/// us into Microsoft's host-naming choices. The contract is: only
/// known-deprecated hosts are flagged Legacy; everything else is treated
/// as "modern" and emitted silently.
pub fn classify_teams_webhook(url: &str) -> TeamsWebhookKind {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let (host_with_port, path) = after_scheme
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((after_scheme, String::new()));
    let host = host_with_port
        .split_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_with_port)
        .to_ascii_lowercase();

    if host == "webhook.office.com" || host.ends_with(".webhook.office.com") {
        return TeamsWebhookKind::Legacy;
    }
    if host == "outlook.office.com" && path.starts_with("/webhook/") {
        return TeamsWebhookKind::Legacy;
    }
    TeamsWebhookKind::Workflow
}

/// Emit a `tracing::warn!` flagging that the user's Teams webhook URL is
/// the deprecated MessageCard form. Public so callers (e.g. config
/// validation) can pre-warn without going through `send_teams`.
pub fn warn_legacy_teams_webhook() {
    tracing::warn!(
        target: "anodizer::announce::teams",
        "Teams webhook URL is a legacy *.webhook.office.com / outlook.office.com \
         MessageCard connector. Microsoft is deprecating these end of 2026 — \
         migrate to a Power Automate Workflow URL (Adaptive Card-native). \
         See https://learn.microsoft.com/en-us/microsoftteams/platform/m365-apps/connectors/connectors-overview"
    );
}

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

/// Build a Microsoft Teams Adaptive Card payload with optional title, color, and icon.
///
/// Color handling: Teams ignores the legacy MessageCard `themeColor` field on
/// Adaptive Card payloads. When a color is configured we instead wrap the
/// title block in a `Container` with `style: "emphasis"` so the configured
/// color visually accents the header. The raw hex value is emitted as a
/// `msteams` metadata field on the card so MessageCard-style consumers that
/// inspect both formats still see it.
pub(crate) fn teams_payload(message: &str, opts: &TeamsOptions<'_>) -> String {
    let title_block = match (opts.title, opts.icon_url) {
        (Some(title), Some(icon)) => Some(json!({
            "type": "ColumnSet",
            "columns": [
                {
                    "type": "Column",
                    "width": "auto",
                    "items": [{
                        "type": "Image",
                        "url": icon,
                        "size": "Small",
                        "style": "Person",
                    }]
                },
                {
                    "type": "Column",
                    "width": "stretch",
                    "items": [{
                        "type": "TextBlock",
                        "text": title,
                        "weight": "Bolder",
                        "size": "Medium",
                        "wrap": true,
                    }]
                }
            ]
        })),
        (Some(title), None) => Some(json!({
            "type": "TextBlock",
            "text": title,
            "weight": "Bolder",
            "size": "Medium",
            "wrap": true,
        })),
        (None, Some(icon)) => Some(json!({
            "type": "Image",
            "url": icon,
            "size": "Small",
        })),
        (None, None) => None,
    };

    let mut body_items: Vec<serde_json::Value> = Vec::new();
    if let Some(header) = title_block {
        if opts.color.is_some() {
            body_items.push(json!({
                "type": "Container",
                "style": "emphasis",
                "bleed": true,
                "items": [header],
            }));
        } else {
            body_items.push(header);
        }
    }
    body_items.push(json!({
        "type": "TextBlock",
        "text": message,
        "wrap": true,
    }));

    let mut card = serde_json::Map::new();
    card.insert(
        "$schema".into(),
        json!("http://adaptivecards.io/schemas/adaptive-card.json"),
    );
    card.insert("type".into(), json!("AdaptiveCard"));
    card.insert("version".into(), json!("1.4"));
    card.insert("body".into(), json!(body_items));
    if let Some(color) = opts.color {
        card.insert("msteams".into(), json!({ "themeColor": color }));
    }

    json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": serde_json::Value::Object(card),
        }],
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to a Microsoft Teams incoming webhook using an Adaptive Card.
///
/// `policy` controls retry behaviour for transport-level / 5xx / 429 failures.
///
/// Both legacy (`*.webhook.office.com`, `outlook.office.com/webhook/...`)
/// and Workflow (Power Automate `logic.azure.com` / `azure-api.net`) URLs
/// are accepted. Legacy URLs still POST, but trigger a `tracing::warn!`
/// referencing Microsoft's deprecation note so the user knows to migrate
/// before the end-of-2026 cutoff. Workflow URLs are silent.
pub fn send_teams(
    webhook_url: &str,
    message: &str,
    opts: &TeamsOptions<'_>,
    policy: &RetryPolicy,
) -> Result<()> {
    if classify_teams_webhook(webhook_url) == TeamsWebhookKind::Legacy {
        warn_legacy_teams_webhook();
    }
    let payload = teams_payload(message, opts);
    post_json(webhook_url, &payload, "teams", policy)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_teams_payload_structure() {
        let opts = TeamsOptions {
            title: None,
            color: None,
            icon_url: None,
        };
        let payload = teams_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["type"], "message");
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            attachments[0]["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        let content = &attachments[0]["content"];
        assert_eq!(content["type"], "AdaptiveCard");
        assert_eq!(content["version"], "1.4");
        let body = content["body"].as_array().unwrap();
        assert_eq!(body.len(), 1);
        assert_eq!(body[0]["type"], "TextBlock");
        assert_eq!(body[0]["text"], "myapp v1.0.0 released!");
        assert_eq!(body[0]["wrap"], true);
    }

    #[test]
    fn test_teams_payload_with_title() {
        let opts = TeamsOptions {
            title: Some("Release Announcement"),
            color: None,
            icon_url: None,
        };
        let payload = teams_payload("v2.0 is out!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0]["text"], "Release Announcement");
        assert_eq!(body[0]["weight"], "Bolder");
        assert_eq!(body[1]["text"], "v2.0 is out!");
    }

    #[test]
    fn test_teams_payload_with_color() {
        // No title, but color set: color is recorded on the card via the
        // msteams extension. Outer envelope must NOT carry themeColor since
        // Teams ignores it on Adaptive Card payloads.
        let opts = TeamsOptions {
            title: None,
            color: Some("0076D7"),
            icon_url: None,
        };
        let payload = teams_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(json.get("themeColor").is_none());
        assert_eq!(
            json["attachments"][0]["content"]["msteams"]["themeColor"],
            "0076D7"
        );
    }

    #[test]
    fn test_teams_payload_with_title_and_color() {
        // With a title and a color, the title block is wrapped in an
        // emphasis Container so the color visually accents the header.
        let opts = TeamsOptions {
            title: Some("New Release"),
            color: Some("FF0000"),
            icon_url: None,
        };
        let payload = teams_payload("v3.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(json.get("themeColor").is_none());
        let card = &json["attachments"][0]["content"];
        assert_eq!(card["msteams"]["themeColor"], "FF0000");
        let body = card["body"].as_array().unwrap();
        assert_eq!(body[0]["type"], "Container");
        assert_eq!(body[0]["style"], "emphasis");
        assert_eq!(body[0]["items"][0]["text"], "New Release");
        assert_eq!(body[1]["text"], "v3.0");
    }

    #[test]
    fn test_teams_payload_with_icon_url() {
        let opts = TeamsOptions {
            title: Some("Release"),
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = teams_payload("v1.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        let first = &body[0];
        assert_eq!(first["type"], "ColumnSet");
        let columns = first["columns"].as_array().unwrap();
        assert_eq!(columns[0]["items"][0]["type"], "Image");
        assert_eq!(
            columns[0]["items"][0]["url"],
            "https://example.com/icon.png"
        );
        assert_eq!(columns[0]["items"][0]["style"], "Person");
        assert_eq!(columns[1]["items"][0]["type"], "TextBlock");
        assert_eq!(columns[1]["items"][0]["text"], "Release");
    }

    // ---- classify_teams_webhook ----
    //
    // Microsoft is deprecating MessageCard webhooks served by
    // *.webhook.office.com (and the older outlook.office.com/webhook/...
    // form). Power Automate Workflow URLs (logic.azure.com / azure-api.net)
    // are the new shape. GoReleaser-imported configs may still carry the
    // legacy form; anodizer accepts both, but emits a warn on legacy URLs
    // so users know to migrate before Microsoft cuts the path.

    #[test]
    fn test_classify_teams_webhook_legacy_outlook_office() {
        let url = "https://outlook.office.com/webhook/abc-def/IncomingWebhook/xyz/123";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Legacy);
    }

    #[test]
    fn test_classify_teams_webhook_legacy_tenant_webhook_office() {
        let url = "https://contoso.webhook.office.com/webhookb2/abc@def/IncomingWebhook/xyz/123";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Legacy);
    }

    #[test]
    fn test_classify_teams_webhook_legacy_webhook_office_com_subdomain() {
        let url = "https://prod-12.webhook.office.com/webhookb2/abc/IncomingWebhook/xyz";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Legacy);
    }

    #[test]
    fn test_classify_teams_webhook_workflow_logic_azure() {
        let url = "https://prod-12.eastus.logic.azure.com:443/workflows/abc/triggers/manual/paths/invoke?api-version=2016-06-01&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=xyz";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Workflow);
    }

    #[test]
    fn test_classify_teams_webhook_workflow_azure_api_net() {
        let url = "https://contoso.azure-api.net/teams/notify";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Workflow);
    }

    #[test]
    fn test_classify_teams_webhook_unknown_host_treated_as_workflow() {
        let url = "https://teams-proxy.internal.example/notify";
        assert_eq!(classify_teams_webhook(url), TeamsWebhookKind::Workflow);
    }

    #[test]
    fn test_teams_payload_with_icon_url_only() {
        let opts = TeamsOptions {
            title: None,
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = teams_payload("v1.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        assert_eq!(body[0]["type"], "Image");
        assert_eq!(body[0]["url"], "https://example.com/icon.png");
        assert_eq!(body[0]["size"], "Small");
        // No "style": "Person" when icon is standalone (no title context).
        assert_eq!(body[1]["type"], "TextBlock");
        assert_eq!(body[1]["text"], "v1.0");
    }
}
