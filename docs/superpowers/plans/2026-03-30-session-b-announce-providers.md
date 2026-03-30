# Session B: Announce Providers — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Achieve GoReleaser parity for all announce providers — fix 4 gaps in existing providers, verify Slack wiring, rewrite SMTP to use real SMTP transport, and add 7 new social/community providers.

**Architecture:** Each provider is a standalone module in `crates/stage-announce/src/` with a config struct in `crates/core/src/config.rs`, a payload builder + send function, and wiring in `crates/stage-announce/src/lib.rs`. All HTTP providers use `reqwest::blocking::Client`. SMTP uses the `lettre` crate. Auth credentials come from environment variables matching GoReleaser's names.

**Tech Stack:** Rust, reqwest (HTTP), lettre (SMTP), serde_json (payloads), anodize-core (templates/context)

---

## File Map

**Modify:**
- `crates/core/src/config.rs` — Add `skip` to `AnnounceConfig`, `icon_url` to `TeamsAnnounce`, `title_template` to `MattermostAnnounce`, `expected_status_codes` to `WebhookConfig`, 7 new provider config structs, add new providers to `AnnounceConfig`
- `crates/stage-announce/src/lib.rs` — Add `skip` evaluation, wire `icon_url`/`title_template`, fix webhook status check, template-render Slack blocks/attachments, add wiring for all 7 new providers
- `crates/stage-announce/src/teams.rs` — Add `icon_url` to `TeamsOptions` and payload
- `crates/stage-announce/src/mattermost.rs` — Add `title` to `MattermostOptions` and payload
- `crates/stage-announce/src/webhook.rs` — Add status code validation, accept `expected_status_codes`
- `crates/stage-announce/src/email.rs` — Replace sendmail pipe with lettre SMTP transport
- `crates/stage-announce/Cargo.toml` — Add `lettre` dependency

**Create:**
- `crates/stage-announce/src/reddit.rs`
- `crates/stage-announce/src/twitter.rs`
- `crates/stage-announce/src/mastodon.rs`
- `crates/stage-announce/src/bluesky.rs`
- `crates/stage-announce/src/linkedin.rs`
- `crates/stage-announce/src/opencollective.rs`
- `crates/stage-announce/src/discourse.rs`

---

### Task 1: announce.skip (template-conditional)

**Files:**
- Modify: `crates/core/src/config.rs` (AnnounceConfig struct)
- Modify: `crates/stage-announce/src/lib.rs` (AnnounceStage::run)

**Context:** GoReleaser's `Announce` struct has a `Skip string` field evaluated via `tmpl.New(ctx).Bool()`. When it renders to `"true"`, the entire announce stage is skipped. Our `AnnounceConfig` has no `skip` field — announce is only skipped when the entire config section is absent.

- [ ] **Step 1: Add `skip` field to AnnounceConfig**

In `crates/core/src/config.rs`, add to the `AnnounceConfig` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AnnounceConfig {
    /// Template-conditional skip: if rendered to "true", skip the entire announce stage.
    pub skip: Option<String>,
    pub discord: Option<DiscordAnnounce>,
    // ... existing fields unchanged ...
}
```

- [ ] **Step 2: Write failing test for skip behavior**

In `crates/stage-announce/src/lib.rs` tests section, add:

```rust
#[test]
fn test_announce_skip_true_skips_all() {
    let announce = AnnounceConfig {
        skip: Some("true".to_string()),
        discord: Some(DiscordAnnounce {
            enabled: Some(true),
            webhook_url: Some("https://discord.invalid/webhook".to_string()),
            message_template: Some("test".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    // Should succeed without attempting any provider (discord URL is invalid).
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_announce_skip_false_does_not_skip() {
    let announce = AnnounceConfig {
        skip: Some("false".to_string()),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_announce_skip_template_evaluated() {
    let announce = AnnounceConfig {
        skip: Some("{{ .IsNightly }}".to_string()),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("IsNightly", "true");
    // Should skip because IsNightly renders to "true".
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p anodize-stage-announce test_announce_skip -- --nocapture`
Expected: Compilation error (skip field doesn't exist yet) or test failure.

- [ ] **Step 4: Implement skip evaluation in AnnounceStage::run**

In `crates/stage-announce/src/lib.rs`, at the top of the `run` method, after extracting the announce config, add skip evaluation:

```rust
fn run(&self, ctx: &mut Context) -> Result<()> {
    let log = ctx.logger("announce");
    let announce = match ctx.config.announce.clone() {
        Some(a) => a,
        None => {
            log.status("no announce config — skipping");
            return Ok(());
        }
    };

    // Evaluate template-conditional skip.
    if let Some(ref skip_tmpl) = announce.skip {
        let rendered = ctx.render_template(skip_tmpl)?;
        if matches!(rendered.trim(), "true" | "1") {
            log.status("announce.skip evaluated to true — skipping");
            return Ok(());
        }
    }

    // ... rest of provider dispatch unchanged ...
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass, including the 3 new skip tests.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add template-conditional announce.skip field"
```

---

### Task 2: Teams icon_url

**Files:**
- Modify: `crates/core/src/config.rs` (TeamsAnnounce struct)
- Modify: `crates/stage-announce/src/teams.rs` (TeamsOptions, payload builder)
- Modify: `crates/stage-announce/src/lib.rs` (Teams wiring)

**Context:** GoReleaser's Teams config has `IconURL string` (default: GoReleaser avatar). It appears in the MessageCard section's `ActivityImage`. Our `TeamsAnnounce` and `TeamsOptions` lack this field entirely.

- [ ] **Step 1: Add `icon_url` to `TeamsAnnounce` config**

In `crates/core/src/config.rs`, modify `TeamsAnnounce`:

```rust
pub struct TeamsAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
    pub title_template: Option<String>,
    pub color: Option<String>,
    /// Optional icon URL displayed in the card header
    pub icon_url: Option<String>,
}
```

- [ ] **Step 2: Write failing test for icon_url in payload**

In `crates/stage-announce/src/teams.rs`, add test:

```rust
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
    // Should have a ColumnSet with an Image column for the icon.
    let first = &body[0];
    assert_eq!(first["type"], "ColumnSet");
    let columns = first["columns"].as_array().unwrap();
    assert_eq!(columns[0]["items"][0]["type"], "Image");
    assert_eq!(columns[0]["items"][0]["url"], "https://example.com/icon.png");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p anodize-stage-announce test_teams_payload_with_icon_url -- --nocapture`
Expected: Compilation error (icon_url not in TeamsOptions).

- [ ] **Step 4: Add icon_url to TeamsOptions and update payload builder**

In `crates/stage-announce/src/teams.rs`:

```rust
pub struct TeamsOptions<'a> {
    pub title: Option<&'a str>,
    pub color: Option<&'a str>,
    pub icon_url: Option<&'a str>,
}
```

Update `teams_payload` to include the icon as a header ColumnSet when provided:

```rust
pub(crate) fn teams_payload(message: &str, opts: &TeamsOptions<'_>) -> String {
    let mut body_items: Vec<serde_json::Value> = Vec::new();

    // If we have both a title and icon, render as a ColumnSet header.
    // If only title, render as a TextBlock.
    match (opts.title, opts.icon_url) {
        (Some(title), Some(icon)) => {
            body_items.push(json!({
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
            }));
        }
        (Some(title), None) => {
            body_items.push(json!({
                "type": "TextBlock",
                "text": title,
                "weight": "Bolder",
                "size": "Medium",
                "wrap": true,
            }));
        }
        (None, Some(icon)) => {
            body_items.push(json!({
                "type": "Image",
                "url": icon,
                "size": "Small",
            }));
        }
        (None, None) => {}
    }

    body_items.push(json!({
        "type": "TextBlock",
        "text": message,
        "wrap": true,
    }));

    let card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": body_items,
    });

    let mut outer = json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": card,
        }],
    });

    if let Some(color) = opts.color {
        outer["themeColor"] = json!(color);
    }

    outer.to_string()
}
```

- [ ] **Step 5: Fix existing tests for new field**

Update all existing `TeamsOptions` instantiations in tests to include `icon_url: None`. Also update `crates/stage-announce/src/lib.rs` Teams wiring:

```rust
// In the Teams section of run():
let icon_url = render_optional(ctx, cfg.icon_url.as_deref())?;
let opts = teams::TeamsOptions {
    title: title.as_deref(),
    color: color.as_deref(),
    icon_url: icon_url.as_deref(),
};
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/teams.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add icon_url to Teams provider"
```

---

### Task 3: Mattermost title_template

**Files:**
- Modify: `crates/core/src/config.rs` (MattermostAnnounce struct)
- Modify: `crates/stage-announce/src/mattermost.rs` (MattermostOptions, payload)
- Modify: `crates/stage-announce/src/lib.rs` (Mattermost wiring)

**Context:** GoReleaser's Mattermost has `TitleTemplate string` (default: `"{{ .ProjectName }} {{ .Tag }} is out!"`). The title goes into the attachment's `title` field. Our MattermostAnnounce struct lacks `title_template`.

- [ ] **Step 1: Add `title_template` to MattermostAnnounce**

In `crates/core/src/config.rs`:

```rust
pub struct MattermostAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub channel: Option<String>,
    pub username: Option<String>,
    pub icon_url: Option<String>,
    pub icon_emoji: Option<String>,
    pub color: Option<String>,
    pub message_template: Option<String>,
    /// Optional title template for the Mattermost attachment
    pub title_template: Option<String>,
}
```

- [ ] **Step 2: Write failing test**

In `crates/stage-announce/src/mattermost.rs`, add:

```rust
#[test]
fn test_mattermost_payload_with_title() {
    let opts = MattermostOptions {
        channel: None,
        username: None,
        icon_url: None,
        icon_emoji: None,
        color: None,
        title: Some("myapp v2.0 is out!"),
    };
    let payload = mattermost_payload("Check the release notes.", &opts);
    let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let attachments = json["attachments"].as_array().unwrap();
    assert_eq!(attachments[0]["title"], "myapp v2.0 is out!");
    assert_eq!(attachments[0]["text"], "Check the release notes.");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p anodize-stage-announce test_mattermost_payload_with_title -- --nocapture`
Expected: Compilation error (title not in MattermostOptions).

- [ ] **Step 4: Add title to MattermostOptions and payload builder**

In `crates/stage-announce/src/mattermost.rs`:

```rust
pub struct MattermostOptions<'a> {
    pub channel: Option<&'a str>,
    pub username: Option<&'a str>,
    pub icon_url: Option<&'a str>,
    pub icon_emoji: Option<&'a str>,
    pub color: Option<&'a str>,
    pub title: Option<&'a str>,
}
```

In `mattermost_payload`, add `title` to the attachment object:

```rust
// In the attachment object construction:
let mut attachment = json!({
    "text": message,
});
if let Some(title) = opts.title {
    attachment["title"] = json!(title);
}
if let Some(color) = opts.color {
    attachment["color"] = json!(color);
}
```

- [ ] **Step 5: Fix existing tests and wiring**

Update all `MattermostOptions` instantiations in tests to include `title: None`.

In `crates/stage-announce/src/lib.rs` Mattermost section, add:

```rust
let title = render_optional(ctx, cfg.title_template.as_deref())?;
// ... in opts:
let opts = mattermost::MattermostOptions {
    channel: channel.as_deref(),
    username: username.as_deref(),
    icon_url: icon_url.as_deref(),
    icon_emoji: icon_emoji.as_deref(),
    color: color.as_deref(),
    title: title.as_deref(),
};
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/mattermost.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add title_template to Mattermost provider"
```

---

### Task 4: Webhook expected_status_codes

**Files:**
- Modify: `crates/core/src/config.rs` (WebhookConfig struct)
- Modify: `crates/stage-announce/src/webhook.rs` (send_webhook)
- Modify: `crates/stage-announce/src/lib.rs` (webhook wiring)

**Context:** GoReleaser's Webhook has `ExpectedStatusCodes []int` (default: `[200, 201, 202, 204]`). Our webhook sends the request but doesn't validate the response status code.

- [ ] **Step 1: Add `expected_status_codes` to WebhookConfig**

In `crates/core/src/config.rs`:

```rust
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub endpoint_url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub content_type: Option<String>,
    pub message_template: Option<String>,
    pub skip_tls_verify: Option<bool>,
    /// HTTP status codes to accept as success (default: [200, 201, 202, 204])
    #[serde(default)]
    pub expected_status_codes: Vec<u16>,
}
```

- [ ] **Step 2: Write failing test for status validation**

In `crates/stage-announce/src/webhook.rs`, add:

```rust
#[test]
fn test_send_webhook_validates_status_codes() {
    // This is a unit test for the status check logic, not a network test.
    let expected = vec![200, 201, 204];
    assert!(is_expected_status(200, &expected));
    assert!(is_expected_status(201, &expected));
    assert!(is_expected_status(204, &expected));
    assert!(!is_expected_status(500, &expected));
    assert!(!is_expected_status(403, &expected));
}

#[test]
fn test_default_expected_status_codes() {
    let defaults = default_expected_status_codes();
    assert_eq!(defaults, vec![200, 201, 202, 204]);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p anodize-stage-announce test_send_webhook_validates -- --nocapture`
Expected: Compilation error (function doesn't exist).

- [ ] **Step 4: Implement status validation in webhook.rs**

In `crates/stage-announce/src/webhook.rs`, update `send_webhook` to accept and check expected status codes:

```rust
pub(crate) fn default_expected_status_codes() -> Vec<u16> {
    vec![200, 201, 202, 204]
}

pub(crate) fn is_expected_status(status: u16, expected: &[u16]) -> bool {
    expected.contains(&status)
}

/// POST to a generic HTTP webhook endpoint with status code validation.
pub fn send_webhook(
    url: &str,
    message: &str,
    headers: &HashMap<String, String>,
    content_type: &str,
    skip_tls: bool,
    expected_status_codes: &[u16],
) -> Result<()> {
    let client = if skip_tls {
        reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?
    } else {
        reqwest::blocking::Client::new()
    };

    let body = webhook_body(message, content_type);

    let mut req = client.post(url).header("Content-Type", content_type);
    for (k, v) in headers {
        req = req.header(k, v);
    }

    let resp = req.body(body).send()?;
    let status = resp.status().as_u16();

    if !is_expected_status(status, expected_status_codes) {
        let body = resp.text().unwrap_or_default();
        anyhow::bail!(
            "webhook returned unexpected status {status} (expected one of {expected_status_codes:?}): {body}"
        );
    }

    Ok(())
}
```

- [ ] **Step 5: Update lib.rs wiring**

In the webhook section of `AnnounceStage::run`:

```rust
let expected_codes = if cfg.expected_status_codes.is_empty() {
    webhook::default_expected_status_codes()
} else {
    cfg.expected_status_codes.clone()
};
dispatch(ctx, "webhook", &message, || {
    webhook::send_webhook(&url, &message, &headers, &content_type, skip_tls, &expected_codes)
})?;
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/webhook.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add expected_status_codes to webhook provider"
```

---

### Task 5: Slack blocks/attachments template rendering

**Files:**
- Modify: `crates/stage-announce/src/lib.rs` (Slack wiring)

**Context:** GoReleaser templates Slack blocks/attachments — it marshals the YAML to JSON, applies template rendering, then parses the result. Our implementation passes the raw `serde_json::Value` through without template rendering. Template variables like `{{ .Tag }}` inside blocks won't expand.

- [ ] **Step 1: Write failing test**

In `crates/stage-announce/src/lib.rs` tests:

```rust
#[test]
fn test_slack_blocks_template_rendering() {
    let blocks_json = serde_json::json!([{
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": "{{ .ProjectName }} {{ .Tag }} is out!"
        }
    }]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(true),
            webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
            message_template: None,
            blocks: Some(blocks_json),
            attachments: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v2.0.0");
    // Dry-run should succeed and the template should be rendered.
    // We verify by checking that the stage doesn't error when templates are present.
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}
```

- [ ] **Step 2: Implement template rendering for blocks/attachments**

In `crates/stage-announce/src/lib.rs`, in the Slack section, template-render the blocks and attachments JSON strings before passing them through:

```rust
// Template-render blocks/attachments JSON so variables like {{ .Tag }} expand.
let blocks = match &cfg.blocks {
    Some(val) => {
        let rendered = ctx.render_template(&val.to_string())?;
        Some(serde_json::from_str::<serde_json::Value>(&rendered)?)
    }
    None => None,
};
let attachments = match &cfg.attachments {
    Some(val) => {
        let rendered = ctx.render_template(&val.to_string())?;
        Some(serde_json::from_str::<serde_json::Value>(&rendered)?)
    }
    None => None,
};
```

- [ ] **Step 3: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stage-announce/src/lib.rs
git commit -m "fix(announce): template-render Slack blocks/attachments"
```

---

### Task 6: SMTP email transport (replace sendmail)

**Files:**
- Modify: `crates/stage-announce/Cargo.toml` (add lettre)
- Modify: `crates/core/src/config.rs` (EmailAnnounce struct)
- Rewrite: `crates/stage-announce/src/email.rs`
- Modify: `crates/stage-announce/src/lib.rs` (email wiring)

**Context:** GoReleaser uses a real SMTP dialer (gomail) with host, port, username, password (from env `SMTP_PASSWORD`), and `insecure_skip_verify`. Our current implementation pipes to `sendmail -t` or `msmtp -t`. We need to replace this with proper SMTP via the `lettre` crate, while keeping sendmail as a fallback when no SMTP host is configured.

- [ ] **Step 1: Add lettre dependency**

In `crates/stage-announce/Cargo.toml`, add:

```toml
lettre = { version = "0.11", default-features = false, features = ["builder", "hostname", "smtp-transport", "rustls-tls"] }
```

Also add to the workspace `Cargo.toml` if needed:

```toml
lettre = { version = "0.11", default-features = false, features = ["builder", "hostname", "smtp-transport", "rustls-tls"] }
```

- [ ] **Step 2: Expand EmailAnnounce config**

In `crates/core/src/config.rs`, replace `EmailAnnounce`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct EmailAnnounce {
    pub enabled: Option<bool>,
    /// SMTP server hostname (if not set, falls back to sendmail/msmtp)
    pub host: Option<String>,
    /// SMTP server port (default: 587)
    pub port: Option<u16>,
    /// SMTP username (can also be set via SMTP_USERNAME env)
    pub username: Option<String>,
    /// Sender email address
    pub from: Option<String>,
    /// Recipient email addresses
    #[serde(default)]
    pub to: Vec<String>,
    pub subject_template: Option<String>,
    /// Body template (called body_template in GoReleaser)
    pub message_template: Option<String>,
    /// Skip TLS certificate verification (default: false)
    pub insecure_skip_verify: Option<bool>,
}
```

- [ ] **Step 3: Write tests for SMTP transport**

In `crates/stage-announce/src/email.rs`, replace the module with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_rfc2822_message_single_recipient() {
        let params = EmailParams {
            from: "release-bot@example.com",
            to: &["dev@example.com".to_string()],
            subject: "myapp v1.0.0 released",
            body: "A new version is available!",
        };
        let msg = build_rfc2822_message(&params).unwrap();
        assert!(msg.contains("From: release-bot@example.com"));
        assert!(msg.contains("To: dev@example.com"));
        assert!(msg.contains("Subject: myapp v1.0.0 released"));
        assert!(msg.contains("A new version is available!"));
    }

    #[test]
    fn test_build_rfc2822_message_multiple_recipients() {
        let params = EmailParams {
            from: "bot@example.com",
            to: &[
                "alice@example.com".to_string(),
                "bob@example.com".to_string(),
            ],
            subject: "Release",
            body: "Done",
        };
        let msg = build_rfc2822_message(&params).unwrap();
        assert!(msg.contains("To: alice@example.com, bob@example.com"));
    }

    #[test]
    fn test_sanitizes_newlines_in_headers() {
        let params = EmailParams {
            from: "bot@example.com",
            to: &["dev@example.com".to_string()],
            subject: "legit\r\nBcc: evil@attacker.com",
            body: "body",
        };
        let msg = build_rfc2822_message(&params).unwrap();
        assert!(!msg.contains("\r\nBcc:"));
    }

    #[test]
    fn test_smtp_params_from_config_defaults() {
        let params = SmtpParams {
            host: "smtp.example.com",
            port: 0,
            username: "user",
            password: "pass",
            insecure_skip_verify: false,
        };
        let effective_port = if params.port == 0 { 587 } else { params.port };
        assert_eq!(effective_port, 587);
    }

    #[test]
    fn test_smtp_params_custom_port() {
        let params = SmtpParams {
            host: "smtp.example.com",
            port: 465,
            username: "user",
            password: "pass",
            insecure_skip_verify: false,
        };
        assert_eq!(params.port, 465);
    }
}
```

- [ ] **Step 4: Implement SMTP transport**

Rewrite `crates/stage-announce/src/email.rs`:

```rust
use anodize_core::template::{self, TemplateVars};
use anyhow::{Context, Result};
use chrono::Utc;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};

/// Parameters for rendering the email message (shared by both SMTP and sendmail).
pub struct EmailParams<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
}

/// Parameters for SMTP connection.
pub struct SmtpParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub password: &'a str,
    pub insecure_skip_verify: bool,
}

fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// Build an RFC 2822 message for sendmail fallback.
const RFC2822_TEMPLATE: &str = "\
From: {{ from }}\r
To: {{ to }}\r
Subject: {{ subject }}\r
MIME-Version: 1.0\r
Content-Type: text/plain; charset=utf-8\r
Date: {{ date }}\r
\r
{{ body }}";

pub(crate) fn build_rfc2822_message(params: &EmailParams<'_>) -> Result<String> {
    let to_header = params
        .to
        .iter()
        .map(|addr| sanitize_header(addr))
        .collect::<Vec<_>>()
        .join(", ");

    let mut vars = TemplateVars::new();
    vars.set("from", &sanitize_header(params.from));
    vars.set("to", &to_header);
    vars.set("subject", &sanitize_header(params.subject));
    vars.set(
        "date",
        &Utc::now().format("%a, %d %b %Y %H:%M:%S +0000").to_string(),
    );
    vars.set("body", params.body);

    template::render(RFC2822_TEMPLATE, &vars).context("failed to render RFC 2822 email template")
}

/// Send email via SMTP using lettre.
pub fn send_smtp(params: &EmailParams<'_>, smtp: &SmtpParams<'_>) -> Result<()> {
    let from = sanitize_header(params.from)
        .parse()
        .context("invalid 'from' address")?;

    let mut message_builder = Message::builder().from(from);
    for addr in params.to {
        let to = sanitize_header(addr)
            .parse()
            .context(format!("invalid 'to' address: {addr}"))?;
        message_builder = message_builder.to(to);
    }

    let email = message_builder
        .subject(sanitize_header(params.subject))
        .header(ContentType::TEXT_PLAIN)
        .body(params.body.to_string())
        .context("failed to build email message")?;

    let creds = Credentials::new(smtp.username.to_string(), smtp.password.to_string());

    let port = if smtp.port == 0 { 587 } else { smtp.port };

    let mut transport_builder = SmtpTransport::starttls_relay(smtp.host)
        .context(format!("failed to connect to SMTP server {}", smtp.host))?
        .port(port)
        .credentials(creds);

    if smtp.insecure_skip_verify {
        let tls_params = TlsParameters::builder(smtp.host.to_string())
            .dangerous_accept_invalid_certs(true)
            .build()
            .context("failed to build TLS parameters")?;
        transport_builder = transport_builder.tls(Tls::Required(tls_params));
    }

    let mailer = transport_builder.build();
    mailer
        .send(&email)
        .context("failed to send email via SMTP")?;

    Ok(())
}

/// Send email by piping to sendmail/msmtp (fallback when no SMTP host configured).
pub fn send_sendmail(params: &EmailParams<'_>) -> Result<()> {
    let message = build_rfc2822_message(params)?;

    let (program, args) = if which_exists("sendmail") {
        ("sendmail", vec!["-t"])
    } else if which_exists("msmtp") {
        ("msmtp", vec!["-t"])
    } else {
        anyhow::bail!(
            "announce.email: neither `sendmail` nor `msmtp` found on PATH. \
             Configure SMTP (host/port) or install sendmail/msmtp."
        );
    };

    let output = std::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(message.as_bytes())?;
            }
            child.wait_with_output()
        })
        .with_context(|| format!("failed to run {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{program} exited with {}: {stderr}", output.status);
    }

    Ok(())
}

fn which_exists(program: &str) -> bool {
    anodize_core::util::find_binary(program)
}
```

- [ ] **Step 5: Update lib.rs email wiring**

In `crates/stage-announce/src/lib.rs`, update the email section to read SMTP config and env vars:

```rust
// Email (SMTP or sendmail fallback)
if let Some(cfg) = &announce.email
    && cfg.enabled.unwrap_or(false)
{
    let from = require_rendered(ctx, cfg.from.as_deref(), "email", "from")?;

    if !from.contains('@') {
        anyhow::bail!(
            "announce.email: 'from' address {:?} does not look like a valid email (missing @)",
            from
        );
    }

    if cfg.to.is_empty() {
        anyhow::bail!("announce.email: missing to (recipient list)");
    }

    let subject = ctx.render_template(
        cfg.subject_template
            .as_deref()
            .unwrap_or("{{ .ProjectName }} {{ .Tag }} released"),
    )?;
    let body = render_message(ctx, cfg.message_template.as_deref())?;

    let email_params = email::EmailParams {
        from: &from,
        to: &cfg.to,
        subject: &subject,
        body: &body,
    };

    let log_line = format!("to {}: {}", cfg.to.join(", "), subject);

    if cfg.host.is_some() {
        // SMTP transport
        let host = cfg.host.as_deref().unwrap();
        let port = cfg.port.unwrap_or(0);
        let username = cfg
            .username
            .as_deref()
            .or_else(|| std::env::var("SMTP_USERNAME").ok().as_deref().map(|_| ()))
            .map(|_| ())  // placeholder — see actual logic below
            ;
        // Read SMTP credentials from config or environment
        let smtp_username = cfg
            .username
            .clone()
            .or_else(|| std::env::var("SMTP_USERNAME").ok())
            .unwrap_or_default();
        let smtp_password = std::env::var("SMTP_PASSWORD")
            .map_err(|_| anyhow::anyhow!("announce.email: SMTP_PASSWORD env var is required for SMTP transport"))?;
        let insecure = cfg.insecure_skip_verify.unwrap_or(false);

        let smtp_params = email::SmtpParams {
            host,
            port,
            username: &smtp_username,
            password: &smtp_password,
            insecure_skip_verify: insecure,
        };

        dispatch(ctx, "email (smtp)", &log_line, || {
            email::send_smtp(&email_params, &smtp_params)
        })?;
    } else {
        // Sendmail fallback
        dispatch(ctx, "email", &log_line, || {
            email::send_sendmail(&email_params)
        })?;
    }
}
```

Note: Clean up the `username` logic above — the actual code should be:

```rust
let smtp_username = cfg
    .username
    .clone()
    .or_else(|| std::env::var("SMTP_USERNAME").ok())
    .unwrap_or_default();
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass. Existing email dry-run tests still work because they don't hit the SMTP path.

- [ ] **Step 7: Commit**

```bash
git add crates/stage-announce/Cargo.toml crates/core/src/config.rs crates/stage-announce/src/email.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): replace sendmail with SMTP transport via lettre"
```

---

### Task 7: Reddit provider

**Files:**
- Modify: `crates/core/src/config.rs` (add RedditAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/reddit.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser Reddit submits a link post to a subreddit. Auth is OAuth2 password grant with application_id + secret + username + password. Env vars: `REDDIT_SECRET`, `REDDIT_PASSWORD`.

- [ ] **Step 1: Add RedditAnnounce config**

In `crates/core/src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RedditAnnounce {
    pub enabled: Option<bool>,
    /// Reddit application (OAuth client) ID
    pub application_id: Option<String>,
    /// Reddit username for posting
    pub username: Option<String>,
    /// Subreddit to post to (without /r/ prefix)
    pub sub: Option<String>,
    pub title_template: Option<String>,
    pub url_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub reddit: Option<RedditAnnounce>,
```

- [ ] **Step 2: Write tests for Reddit payload and validation**

Create `crates/stage-announce/src/reddit.rs`:

```rust
use anyhow::Result;
use std::collections::HashMap;

/// Submit a link post to a Reddit subreddit.
///
/// Auth flow:
/// 1. POST to https://www.reddit.com/api/v1/access_token with Basic Auth
///    (application_id:secret) and grant_type=password, username, password.
/// 2. POST to https://oauth.reddit.com/api/submit with kind=link, sr, title, url.
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

    let token_json: serde_json::Value = token_resp.json()?;
    let access_token = token_json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("reddit: missing access_token in OAuth response"))?;

    // Step 2: Submit link
    let mut form = HashMap::new();
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Network tests can't run in CI, so we test validation and config only.
    // The send function is tested indirectly via dry-run in lib.rs tests.
}
```

- [ ] **Step 3: Add module declaration and wiring in lib.rs**

In `crates/stage-announce/src/lib.rs`, add:

```rust
pub mod reddit;
```

And in the `run` method, add the Reddit section:

```rust
// Reddit
if let Some(cfg) = &announce.reddit
    && cfg.enabled.unwrap_or(false)
{
    let app_id = require_rendered(ctx, cfg.application_id.as_deref(), "reddit", "application_id")?;
    let username = require_rendered(ctx, cfg.username.as_deref(), "reddit", "username")?;
    let sub = require_rendered(ctx, cfg.sub.as_deref(), "reddit", "sub")?;
    let title = ctx.render_template(
        cfg.title_template
            .as_deref()
            .unwrap_or("{{ .ProjectName }} {{ .Tag }} is out!"),
    )?;
    let url = ctx.render_template(
        cfg.url_template
            .as_deref()
            .unwrap_or("{{ .ReleaseURL }}"),
    )?;
    let secret = std::env::var("REDDIT_SECRET")
        .map_err(|_| anyhow::anyhow!("announce.reddit: REDDIT_SECRET env var is required"))?;
    let password = std::env::var("REDDIT_PASSWORD")
        .map_err(|_| anyhow::anyhow!("announce.reddit: REDDIT_PASSWORD env var is required"))?;

    dispatch(ctx, "reddit", &format!("r/{sub}: {title}"), || {
        reddit::send_reddit(&app_id, &secret, &username, &password, &sub, &title, &url)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests for Reddit**

```rust
#[test]
fn test_skips_disabled_reddit() {
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(false),
            application_id: Some("app123".to_string()),
            username: Some("testuser".to_string()),
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_reddit() {
    std::env::set_var("REDDIT_SECRET", "testsecret");
    std::env::set_var("REDDIT_PASSWORD", "testpass");
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(true),
            application_id: Some("app123".to_string()),
            username: Some("testuser".to_string()),
            sub: Some("rust".to_string()),
            title_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
            url_template: Some("{{ .ReleaseURL }}".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ReleaseURL", "https://github.com/org/myapp/releases/tag/v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("REDDIT_SECRET");
    std::env::remove_var("REDDIT_PASSWORD");
}
```

Add `RedditAnnounce` to the test imports.

- [ ] **Step 5: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/reddit.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add Reddit provider"
```

---

### Task 8: Twitter/X provider

**Files:**
- Modify: `crates/core/src/config.rs` (add TwitterAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/twitter.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser posts a tweet via OAuth1 (4 env vars: TWITTER_CONSUMER_KEY, TWITTER_CONSUMER_SECRET, TWITTER_ACCESS_TOKEN, TWITTER_ACCESS_TOKEN_SECRET). Uses Twitter API v1.1 `statuses/update` (deprecated) — we'll use v2 `POST /2/tweets` with OAuth 1.0a which uses the same 4 credentials.

- [ ] **Step 1: Add TwitterAnnounce config**

In `crates/core/src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TwitterAnnounce {
    pub enabled: Option<bool>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub twitter: Option<TwitterAnnounce>,
```

- [ ] **Step 2: Create twitter.rs with OAuth1 signing and send**

Create `crates/stage-announce/src/twitter.rs`:

```rust
use anyhow::Result;
use reqwest::blocking::Client;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Post a tweet using Twitter API v2 with OAuth 1.0a authentication.
///
/// Env vars: TWITTER_CONSUMER_KEY, TWITTER_CONSUMER_SECRET,
///           TWITTER_ACCESS_TOKEN, TWITTER_ACCESS_TOKEN_SECRET
pub fn send_twitter(
    consumer_key: &str,
    consumer_secret: &str,
    access_token: &str,
    access_token_secret: &str,
    message: &str,
) -> Result<()> {
    let url = "https://api.twitter.com/2/tweets";
    let method = "POST";

    let auth_header = build_oauth1_header(
        method,
        url,
        consumer_key,
        consumer_secret,
        access_token,
        access_token_secret,
    )?;

    let body = serde_json::json!({ "text": message });

    let client = Client::new();
    let resp = client
        .post(url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("twitter: API request failed ({status}): {body}");
    }

    Ok(())
}

/// Build an OAuth 1.0a Authorization header.
fn build_oauth1_header(
    method: &str,
    url: &str,
    consumer_key: &str,
    consumer_secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<String> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .to_string();
    let nonce: String = (0..32)
        .map(|_| format!("{:x}", rand_byte()))
        .collect();

    let mut params = BTreeMap::new();
    params.insert("oauth_consumer_key", consumer_key);
    params.insert("oauth_nonce", &nonce);
    params.insert("oauth_signature_method", "HMAC-SHA1");
    params.insert("oauth_timestamp", &timestamp);
    params.insert("oauth_token", token);
    params.insert("oauth_version", "1.0");

    let param_string: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        method,
        percent_encode(url),
        percent_encode(&param_string)
    );

    let signing_key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );

    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(signing_key.as_bytes())
        .map_err(|e| anyhow::anyhow!("HMAC error: {e}"))?;
    mac.update(base_string.as_bytes());
    let signature = base64_encode(&mac.finalize().into_bytes());

    Ok(format!(
        "OAuth oauth_consumer_key=\"{}\", oauth_nonce=\"{}\", oauth_signature=\"{}\", oauth_signature_method=\"HMAC-SHA1\", oauth_timestamp=\"{}\", oauth_token=\"{}\", oauth_version=\"1.0\"",
        percent_encode(consumer_key),
        percent_encode(&nonce),
        percent_encode(&signature),
        percent_encode(&timestamp),
        percent_encode(token),
    ))
}

fn percent_encode(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn rand_byte() -> u8 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    (hasher.finish() & 0xFF) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode_basic() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn test_oauth1_header_format() {
        let header = build_oauth1_header(
            "POST",
            "https://api.twitter.com/2/tweets",
            "consumer_key",
            "consumer_secret",
            "access_token",
            "access_token_secret",
        )
        .unwrap();
        assert!(header.starts_with("OAuth "));
        assert!(header.contains("oauth_consumer_key=\"consumer_key\""));
        assert!(header.contains("oauth_token=\"access_token\""));
        assert!(header.contains("oauth_signature_method=\"HMAC-SHA1\""));
        assert!(header.contains("oauth_version=\"1.0\""));
        assert!(header.contains("oauth_signature="));
    }
}
```

**Note:** This requires `hmac`, `sha1`, and `base64` crates. Add to `Cargo.toml`:

```toml
hmac = "0.12"
sha1 = "0.10"
base64 = "0.22"
```

- [ ] **Step 3: Add module declaration and wiring in lib.rs**

```rust
pub mod twitter;
```

Wiring:

```rust
// Twitter/X
if let Some(cfg) = &announce.twitter
    && cfg.enabled.unwrap_or(false)
{
    let message = render_message(ctx, cfg.message_template.as_deref())?;
    let consumer_key = std::env::var("TWITTER_CONSUMER_KEY")
        .map_err(|_| anyhow::anyhow!("announce.twitter: TWITTER_CONSUMER_KEY env var is required"))?;
    let consumer_secret = std::env::var("TWITTER_CONSUMER_SECRET")
        .map_err(|_| anyhow::anyhow!("announce.twitter: TWITTER_CONSUMER_SECRET env var is required"))?;
    let access_token = std::env::var("TWITTER_ACCESS_TOKEN")
        .map_err(|_| anyhow::anyhow!("announce.twitter: TWITTER_ACCESS_TOKEN env var is required"))?;
    let access_token_secret = std::env::var("TWITTER_ACCESS_TOKEN_SECRET")
        .map_err(|_| anyhow::anyhow!("announce.twitter: TWITTER_ACCESS_TOKEN_SECRET env var is required"))?;

    dispatch(ctx, "twitter", &message, || {
        twitter::send_twitter(&consumer_key, &consumer_secret, &access_token, &access_token_secret, &message)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_twitter() {
    let announce = AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            enabled: Some(false),
            message_template: Some("test".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_twitter() {
    std::env::set_var("TWITTER_CONSUMER_KEY", "ck");
    std::env::set_var("TWITTER_CONSUMER_SECRET", "cs");
    std::env::set_var("TWITTER_ACCESS_TOKEN", "at");
    std::env::set_var("TWITTER_ACCESS_TOKEN_SECRET", "ats");
    let announce = AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            enabled: Some(true),
            message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("TWITTER_CONSUMER_KEY");
    std::env::remove_var("TWITTER_CONSUMER_SECRET");
    std::env::remove_var("TWITTER_ACCESS_TOKEN");
    std::env::remove_var("TWITTER_ACCESS_TOKEN_SECRET");
}
```

Add `TwitterAnnounce` to test imports.

- [ ] **Step 5: Run all tests**

Run: `cargo test -p anodize-stage-announce -- --nocapture`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/config.rs crates/stage-announce/src/twitter.rs crates/stage-announce/src/lib.rs crates/stage-announce/Cargo.toml
git commit -m "feat(announce): add Twitter/X provider with OAuth 1.0a"
```

---

### Task 9: Mastodon provider

**Files:**
- Modify: `crates/core/src/config.rs` (add MastodonAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/mastodon.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser posts a toot via Mastodon API. Config: `server` (instance URL), `message_template`. Env vars: `MASTODON_CLIENT_ID`, `MASTODON_CLIENT_SECRET`, `MASTODON_ACCESS_TOKEN`. Skip if server is empty.

- [ ] **Step 1: Add MastodonAnnounce config**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MastodonAnnounce {
    pub enabled: Option<bool>,
    /// Mastodon instance URL (e.g. "https://mastodon.social")
    pub server: Option<String>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub mastodon: Option<MastodonAnnounce>,
```

- [ ] **Step 2: Create mastodon.rs**

```rust
use anyhow::Result;
use reqwest::blocking::Client;

/// Post a status (toot) to a Mastodon instance.
///
/// Env vars: MASTODON_CLIENT_ID, MASTODON_CLIENT_SECRET, MASTODON_ACCESS_TOKEN
pub fn send_mastodon(server: &str, access_token: &str, message: &str) -> Result<()> {
    let url = format!("{}/api/v1/statuses", server.trim_end_matches('/'));

    let client = Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .form(&[("status", message)])
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("mastodon: API request failed ({status}): {body}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {}
```

- [ ] **Step 3: Add wiring in lib.rs**

```rust
pub mod mastodon;
```

```rust
// Mastodon
if let Some(cfg) = &announce.mastodon
    && cfg.enabled.unwrap_or(false)
{
    let server = require_rendered(ctx, cfg.server.as_deref(), "mastodon", "server")?;
    let message = render_message(ctx, cfg.message_template.as_deref())?;
    let access_token = std::env::var("MASTODON_ACCESS_TOKEN")
        .map_err(|_| anyhow::anyhow!("announce.mastodon: MASTODON_ACCESS_TOKEN env var is required"))?;

    dispatch(ctx, "mastodon", &message, || {
        mastodon::send_mastodon(&server, &access_token, &message)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_mastodon() {
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(false),
            server: Some("https://mastodon.social".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_mastodon() {
    std::env::set_var("MASTODON_ACCESS_TOKEN", "test_token");
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(true),
            server: Some("https://mastodon.social".to_string()),
            message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("MASTODON_ACCESS_TOKEN");
}

#[test]
fn test_missing_mastodon_server_returns_error() {
    std::env::set_var("MASTODON_ACCESS_TOKEN", "test_token");
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(true),
            server: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
    std::env::remove_var("MASTODON_ACCESS_TOKEN");
}
```

Add `MastodonAnnounce` to test imports.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo test -p anodize-stage-announce -- --nocapture`

```bash
git add crates/core/src/config.rs crates/stage-announce/src/mastodon.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add Mastodon provider"
```

---

### Task 10: Bluesky provider

**Files:**
- Modify: `crates/core/src/config.rs` (add BlueskyAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/bluesky.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser uses AT Protocol to post to Bluesky. Two API calls: (1) createSession with username + app password to get JWT tokens, (2) repo.createRecord to post a feed.post. Auto-detects ReleaseURL in message to create a link facet. Env var: `BLUESKY_APP_PASSWORD`. PDS URL hardcoded to `https://bsky.social`.

- [ ] **Step 1: Add BlueskyAnnounce config**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BlueskyAnnounce {
    pub enabled: Option<bool>,
    /// Bluesky handle/username (e.g. "user.bsky.social")
    pub username: Option<String>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub bluesky: Option<BlueskyAnnounce>,
```

- [ ] **Step 2: Create bluesky.rs**

```rust
use anyhow::Result;
use reqwest::blocking::Client;
use serde_json::json;

const PDS_URL: &str = "https://bsky.social";

/// Post to Bluesky via the AT Protocol.
///
/// 1. Create session (login) to get access JWT
/// 2. Create feed post record, with optional link facet for release_url
pub fn send_bluesky(
    username: &str,
    app_password: &str,
    message: &str,
    release_url: Option<&str>,
) -> Result<()> {
    let client = Client::builder()
        .user_agent("anodize/1.0")
        .build()?;

    // Step 1: Create session
    let session_resp = client
        .post(format!("{PDS_URL}/xrpc/com.atproto.server.createSession"))
        .json(&json!({
            "identifier": username,
            "password": app_password,
        }))
        .send()?;

    if !session_resp.status().is_success() {
        let status = session_resp.status();
        let body = session_resp.text().unwrap_or_default();
        anyhow::bail!("bluesky: login failed ({status}): {body}");
    }

    let session: serde_json::Value = session_resp.json()?;
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

    // Add link facet if release_url is found in the message text
    if let Some(url) = release_url {
        if let Some(byte_start) = message.find(url) {
            let byte_end = byte_start + url.len();
            record["facets"] = json!([{
                "index": {
                    "byteStart": byte_start,
                    "byteEnd": byte_end,
                },
                "features": [{
                    "$type": "app.bsky.richtext.facet#link",
                    "uri": url,
                }]
            }]);
        }
    }

    let create_resp = client
        .post(format!("{PDS_URL}/xrpc/com.atproto.repo.createRecord"))
        .bearer_auth(access_jwt)
        .json(&json!({
            "repo": did,
            "collection": "app.bsky.feed.post",
            "record": record,
        }))
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
    use super::*;

    #[test]
    fn test_link_facet_detection() {
        let message = "myapp v1.0.0 is out! Check it out at https://github.com/org/repo/releases/tag/v1.0.0";
        let url = "https://github.com/org/repo/releases/tag/v1.0.0";
        let byte_start = message.find(url).unwrap();
        let byte_end = byte_start + url.len();
        assert_eq!(byte_start, 39);
        assert_eq!(byte_end, 39 + url.len());
    }
}
```

- [ ] **Step 3: Add wiring in lib.rs**

```rust
pub mod bluesky;
```

```rust
// Bluesky
if let Some(cfg) = &announce.bluesky
    && cfg.enabled.unwrap_or(false)
{
    let username = require_rendered(ctx, cfg.username.as_deref(), "bluesky", "username")?;
    let message = render_message(ctx, cfg.message_template.as_deref())?;
    let app_password = std::env::var("BLUESKY_APP_PASSWORD")
        .map_err(|_| anyhow::anyhow!("announce.bluesky: BLUESKY_APP_PASSWORD env var is required"))?;
    let release_url = ctx.template_vars().get("ReleaseURL").map(|s| s.to_string());

    dispatch(ctx, "bluesky", &message, || {
        bluesky::send_bluesky(&username, &app_password, &message, release_url.as_deref())
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_bluesky() {
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(false),
            username: Some("user.bsky.social".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_bluesky() {
    std::env::set_var("BLUESKY_APP_PASSWORD", "test_pass");
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(true),
            username: Some("user.bsky.social".to_string()),
            message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("BLUESKY_APP_PASSWORD");
}
```

Add `BlueskyAnnounce` to test imports.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo test -p anodize-stage-announce -- --nocapture`

```bash
git add crates/core/src/config.rs crates/stage-announce/src/bluesky.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add Bluesky provider"
```

---

### Task 11: LinkedIn provider

**Files:**
- Modify: `crates/core/src/config.rs` (add LinkedInAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/linkedin.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser uses LinkedIn's v2 Share API. Two steps: (1) get profile URN via `/v2/userinfo` (or fallback to `/v2/me`), (2) POST share to `/v2/shares`. Env var: `LINKEDIN_ACCESS_TOKEN`.

- [ ] **Step 1: Add LinkedInAnnounce config**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct LinkedInAnnounce {
    pub enabled: Option<bool>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub linkedin: Option<LinkedInAnnounce>,
```

- [ ] **Step 2: Create linkedin.rs**

```rust
use anyhow::Result;
use reqwest::blocking::Client;
use serde_json::json;

const API_BASE: &str = "https://api.linkedin.com";

/// Post a share to LinkedIn.
///
/// 1. Get profile URN via /v2/userinfo (sub field)
/// 2. POST share to /v2/shares
///
/// Env var: LINKEDIN_ACCESS_TOKEN
pub fn send_linkedin(access_token: &str, message: &str) -> Result<()> {
    let client = Client::new();

    // Step 1: Get profile URN
    let profile_urn = get_profile_urn(&client, access_token)?;

    // Step 2: Create share
    let share = json!({
        "owner": format!("urn:li:person:{profile_urn}"),
        "text": {
            "text": message,
        },
        "distribution": {
            "linkedInDistributionTarget": {}
        }
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

    Ok(())
}

fn get_profile_urn(client: &Client, access_token: &str) -> Result<String> {
    // Try newer /v2/userinfo first
    let resp = client
        .get(format!("{API_BASE}/v2/userinfo"))
        .bearer_auth(access_token)
        .send()?;

    if resp.status().is_success() {
        let json: serde_json::Value = resp.json()?;
        if let Some(sub) = json["sub"].as_str() {
            return Ok(sub.to_string());
        }
    }

    // Fallback to legacy /v2/me
    let resp = client
        .get(format!("{API_BASE}/v2/me"))
        .bearer_auth(access_token)
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("linkedin: failed to get profile ({status}): {body}");
    }

    let json: serde_json::Value = resp.json()?;
    json["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("linkedin: missing 'id' in /v2/me response"))
}

#[cfg(test)]
mod tests {}
```

- [ ] **Step 3: Add wiring in lib.rs**

```rust
pub mod linkedin;
```

```rust
// LinkedIn
if let Some(cfg) = &announce.linkedin
    && cfg.enabled.unwrap_or(false)
{
    let message = render_message(ctx, cfg.message_template.as_deref())?;
    let access_token = std::env::var("LINKEDIN_ACCESS_TOKEN")
        .map_err(|_| anyhow::anyhow!("announce.linkedin: LINKEDIN_ACCESS_TOKEN env var is required"))?;

    dispatch(ctx, "linkedin", &message, || {
        linkedin::send_linkedin(&access_token, &message)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_linkedin() {
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_linkedin() {
    std::env::set_var("LINKEDIN_ACCESS_TOKEN", "test_token");
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(true),
            message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("LINKEDIN_ACCESS_TOKEN");
}
```

Add `LinkedInAnnounce` to test imports.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo test -p anodize-stage-announce -- --nocapture`

```bash
git add crates/core/src/config.rs crates/stage-announce/src/linkedin.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add LinkedIn provider"
```

---

### Task 12: OpenCollective provider

**Files:**
- Modify: `crates/core/src/config.rs` (add OpenCollectiveAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/opencollective.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser uses OpenCollective's GraphQL API. Two mutations: (1) createUpdate with title/html/slug, (2) publishUpdate with the returned ID. Env var: `OPENCOLLECTIVE_TOKEN`. Default message template uses HTML. Skip when slug is empty.

- [ ] **Step 1: Add OpenCollectiveAnnounce config**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct OpenCollectiveAnnounce {
    pub enabled: Option<bool>,
    /// Collective slug (e.g. "my-project")
    pub slug: Option<String>,
    pub title_template: Option<String>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub opencollective: Option<OpenCollectiveAnnounce>,
```

- [ ] **Step 2: Create opencollective.rs**

```rust
use anyhow::Result;
use reqwest::blocking::Client;
use serde_json::json;

const GRAPHQL_URL: &str = "https://api.opencollective.com/graphql/v2";

const DEFAULT_TITLE_TEMPLATE: &str = "{{ .Tag }}";
const DEFAULT_MESSAGE_TEMPLATE: &str =
    r#"{{ .ProjectName }} {{ .Tag }} is out!<br/>Check it out at <a href="{{ .ReleaseURL }}">{{ .ReleaseURL }}</a>"#;

pub fn default_title_template() -> &'static str {
    DEFAULT_TITLE_TEMPLATE
}

pub fn default_message_template() -> &'static str {
    DEFAULT_MESSAGE_TEMPLATE
}

/// Create and publish an update on OpenCollective.
///
/// 1. GraphQL createUpdate mutation
/// 2. GraphQL publishUpdate mutation (audience: ALL)
pub fn send_opencollective(token: &str, slug: &str, title: &str, html: &str) -> Result<()> {
    let client = Client::new();

    // Step 1: Create update
    let create_query = r#"mutation($update: UpdateCreateInput!) {
        createUpdate(update: $update) { id }
    }"#;

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

    let resp_json: serde_json::Value = resp.json()?;
    let update_id = resp_json["data"]["createUpdate"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("opencollective: missing update ID in createUpdate response"))?;

    // Step 2: Publish update
    let publish_query = r#"mutation($id: String!, $audience: UpdateAudience!) {
        publishUpdate(id: $id, notificationAudience: $audience) { id }
    }"#;

    let publish_body = json!({
        "query": publish_query,
        "variables": {
            "id": update_id,
            "audience": "ALL"
        }
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
mod tests {}
```

- [ ] **Step 3: Add wiring in lib.rs**

```rust
pub mod opencollective;
```

```rust
// OpenCollective
if let Some(cfg) = &announce.opencollective
    && cfg.enabled.unwrap_or(false)
{
    let slug = require_rendered(ctx, cfg.slug.as_deref(), "opencollective", "slug")?;
    let title = ctx.render_template(
        cfg.title_template
            .as_deref()
            .unwrap_or(opencollective::default_title_template()),
    )?;
    let html = ctx.render_template(
        cfg.message_template
            .as_deref()
            .unwrap_or(opencollective::default_message_template()),
    )?;
    let token = std::env::var("OPENCOLLECTIVE_TOKEN")
        .map_err(|_| anyhow::anyhow!("announce.opencollective: OPENCOLLECTIVE_TOKEN env var is required"))?;

    dispatch(ctx, "opencollective", &title, || {
        opencollective::send_opencollective(&token, &slug, &title, &html)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_opencollective() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(false),
            slug: Some("my-project".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_opencollective() {
    std::env::set_var("OPENCOLLECTIVE_TOKEN", "test_token");
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(true),
            slug: Some("my-project".to_string()),
            title_template: Some("{{ .Tag }}".to_string()),
            message_template: Some("{{ .ProjectName }} released".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("OPENCOLLECTIVE_TOKEN");
}

#[test]
fn test_missing_opencollective_slug_returns_error() {
    std::env::set_var("OPENCOLLECTIVE_TOKEN", "test_token");
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(true),
            slug: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
    std::env::remove_var("OPENCOLLECTIVE_TOKEN");
}
```

Add `OpenCollectiveAnnounce` to test imports.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo test -p anodize-stage-announce -- --nocapture`

```bash
git add crates/core/src/config.rs crates/stage-announce/src/opencollective.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add OpenCollective provider"
```

---

### Task 13: Discourse provider

**Files:**
- Modify: `crates/core/src/config.rs` (add DiscourseAnnounce, add to AnnounceConfig)
- Create: `crates/stage-announce/src/discourse.rs`
- Modify: `crates/stage-announce/src/lib.rs` (add module + wiring)

**Context:** GoReleaser posts a new topic to a Discourse forum. Config: `server` (URL), `category_id` (int), `username` (default "system"), `title_template`, `message_template`. Env var: `DISCOURSE_API_KEY`. Validates server and category_id.

- [ ] **Step 1: Add DiscourseAnnounce config**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscourseAnnounce {
    pub enabled: Option<bool>,
    /// Discourse forum URL (e.g. "https://forum.example.com")
    pub server: Option<String>,
    /// Category ID to post in
    pub category_id: Option<u64>,
    /// Username for the API request (default: "system")
    pub username: Option<String>,
    pub title_template: Option<String>,
    pub message_template: Option<String>,
}
```

Add to `AnnounceConfig`:

```rust
pub discourse: Option<DiscourseAnnounce>,
```

- [ ] **Step 2: Create discourse.rs**

```rust
use anyhow::Result;
use reqwest::blocking::Client;
use serde_json::json;

/// Create a new topic on a Discourse forum.
///
/// Env var: DISCOURSE_API_KEY
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

    let client = Client::new();
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
mod tests {}
```

- [ ] **Step 3: Add wiring in lib.rs**

```rust
pub mod discourse;
```

```rust
// Discourse
if let Some(cfg) = &announce.discourse
    && cfg.enabled.unwrap_or(false)
{
    let server = require_rendered(ctx, cfg.server.as_deref(), "discourse", "server")?;
    let category_id = cfg.category_id.ok_or_else(|| {
        anyhow::anyhow!("announce.discourse: missing category_id")
    })?;
    if category_id == 0 {
        anyhow::bail!("announce.discourse: category_id must be non-zero");
    }
    let username = cfg.username.as_deref().unwrap_or("system");
    let title = ctx.render_template(
        cfg.title_template
            .as_deref()
            .unwrap_or("{{ .ProjectName }} {{ .Tag }} is out!"),
    )?;
    let message = render_message(ctx, cfg.message_template.as_deref())?;
    let api_key = std::env::var("DISCOURSE_API_KEY")
        .map_err(|_| anyhow::anyhow!("announce.discourse: DISCOURSE_API_KEY env var is required"))?;

    dispatch(ctx, "discourse", &title, || {
        discourse::send_discourse(&server, &api_key, username, category_id, &title, &message)
    })?;
}
```

- [ ] **Step 4: Write lib.rs tests**

```rust
#[test]
fn test_skips_disabled_discourse() {
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(false),
            server: Some("https://forum.example.com".to_string()),
            category_id: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_discourse() {
    std::env::set_var("DISCOURSE_API_KEY", "test_key");
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(true),
            server: Some("https://forum.example.com".to_string()),
            category_id: Some(5),
            username: Some("release-bot".to_string()),
            title_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
            message_template: Some("New release".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    std::env::remove_var("DISCOURSE_API_KEY");
}

#[test]
fn test_missing_discourse_server_returns_error() {
    std::env::set_var("DISCOURSE_API_KEY", "test_key");
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(true),
            server: None,
            category_id: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
    std::env::remove_var("DISCOURSE_API_KEY");
}

#[test]
fn test_missing_discourse_category_id_returns_error() {
    std::env::set_var("DISCOURSE_API_KEY", "test_key");
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(true),
            server: Some("https://forum.example.com".to_string()),
            category_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
    std::env::remove_var("DISCOURSE_API_KEY");
}

#[test]
fn test_zero_discourse_category_id_returns_error() {
    std::env::set_var("DISCOURSE_API_KEY", "test_key");
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(true),
            server: Some("https://forum.example.com".to_string()),
            category_id: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
    std::env::remove_var("DISCOURSE_API_KEY");
}
```

Add `DiscourseAnnounce` to test imports.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo test -p anodize-stage-announce -- --nocapture`

```bash
git add crates/core/src/config.rs crates/stage-announce/src/discourse.rs crates/stage-announce/src/lib.rs
git commit -m "feat(announce): add Discourse provider"
```

---

### Task 14: Update parity session index

**Files:**
- Modify: `.claude/specs/parity-session-index.md`

- [ ] **Step 1: Check all Session B boxes**

Mark all Session B items as `[x]` in the parity session index.

- [ ] **Step 2: Commit**

```bash
git add .claude/specs/parity-session-index.md
git commit -m "docs: mark all Session B items complete in parity session index"
```
