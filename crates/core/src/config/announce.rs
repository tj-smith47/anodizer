use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{HumanDuration, StringOrBool, deserialize_string_or_bool_opt};

/// Default overall announce-stage deadline when `announce.deadline` is unset.
///
/// Announce is a best-effort post-publish notification: every configured
/// channel runs concurrently, each bounded by the per-call HTTP/SMTP timeout
/// and the announce retry profile. This caps the *aggregate* stage so a set of
/// unreachable channels (e.g. external endpoints on an egress-firewalled
/// self-hosted runner) cannot accumulate into a multi-minute hang that trips
/// the pipeline timeout AFTER the release already published. Stragglers past
/// this deadline are abandoned with a warning and are not awaited further.
pub const DEFAULT_ANNOUNCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Minimum for the resolved announce deadline.
///
/// An explicit `deadline: "0s"` (or any sub-second value) would let the runner
/// abandon every channel on its first scheduling tick — traffic still leaves,
/// but every result is warned as "did not complete", which is a useless and
/// surprising state. Clamping the resolved deadline up to this minimum makes that
/// footgun unrepresentable: the smallest meaningful aggregate budget is one
/// second, enough for a reachable local relay to report.
pub const MIN_ANNOUNCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

// AnnounceConfig

/// When the announce stage runs, judged from the results of the publishers
/// and blob uploads earlier in the same run:
///
/// - `required_publishers` (default): announce is skipped when a
///   `required: true` publisher failed or its rollback failed.
/// - `all_publishers`: announce is skipped when any publisher, required or
///   not, failed, its rollback failed, was skipped because an earlier
///   required publisher failed, or was held back by the verify-release
///   gate. A publisher skipped for any other reason (not configured,
///   snapshot, dry run, nightly, already published, deselected, `skip`) or
///   waiting on moderation or validation does not stop announce.
/// - `none`: announce always runs.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnnounceGate {
    #[default]
    RequiredPublishers,
    AllPublishers,
    None,
}

/// Announce-stage integrations.
///
/// Message bodies are secret-redacted before send: known secret env values
/// are masked (a real token becomes `$NAME`). Redaction is on by default;
/// `anodizer notify --allow-secrets` opts a single send out for a trusted
/// private channel, while anodizer's own log output stays redacted regardless.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AnnounceConfig {
    /// Template-conditional skip: skip the entire announce stage when the
    /// value is, or a template renders, `true` or `1` (any letter case).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the entire announce stage is
    /// skipped. Render failure hard-errors. `skip:` is the opposite test:
    /// it skips on a truthy render.
    /// An absent, empty or blank `if:` imposes no gate and always runs; the
    /// falsy test applies to what a non-blank gate renders.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When the announce stage runs, judged from the results of the
    /// publishers and blob uploads earlier in the same run:
    /// `required_publishers` (default) skips it when a `required: true`
    /// publisher failed or its rollback failed; `all_publishers` also skips
    /// it when any other publisher failed, its rollback failed, was skipped
    /// after an earlier required failure, or was held back by the
    /// verify-release gate; `none` always runs it. A publisher skipped for
    /// another reason, or waiting on moderation, does not stop announce.
    #[serde(default)]
    pub gate_on: AnnounceGate,
    /// Overall wall-clock deadline for the announce stage (e.g. `"90s"`,
    /// `"2m"`). Optional; defaults to `90s`. A value below one second is
    /// raised to one second.
    ///
    /// Announcers run concurrently; any still running when this deadline
    /// elapses is abandoned with a warning. This bounds the stage so
    /// unreachable channels cannot add up to a hang that trips the pipeline
    /// timeout after the publishers have already published.
    /// Raise it only if a slow-but-reachable channel legitimately needs longer.
    pub deadline: Option<HumanDuration>,
    /// Discord announcement configuration.
    pub discord: Option<DiscordAnnounce>,
    /// Discourse announcement configuration.
    pub discourse: Option<DiscourseAnnounce>,
    /// Slack announcement configuration.
    pub slack: Option<SlackAnnounce>,
    /// Generic webhook announcement configuration.
    pub webhook: Option<WebhookConfig>,
    /// Telegram announcement configuration.
    pub telegram: Option<TelegramAnnounce>,
    /// Microsoft Teams announcement configuration.
    pub teams: Option<TeamsAnnounce>,
    /// Mattermost announcement configuration.
    pub mattermost: Option<MattermostAnnounce>,
    /// Email announcement configuration. accepts the
    /// historical `smtp:` key as an alias because the field was renamed
    /// `smtp:` -> `email:` in v1.21+ and kept the alias for migration.
    /// Keeping the alias avoids forcing a re-yaml of legacy configs.
    #[serde(alias = "smtp")]
    pub email: Option<EmailAnnounce>,
    /// Reddit announcement configuration.
    pub reddit: Option<RedditAnnounce>,
    /// Twitter/X announcement configuration.
    pub twitter: Option<TwitterAnnounce>,
    /// Mastodon announcement configuration.
    pub mastodon: Option<MastodonAnnounce>,
    /// Bluesky announcement configuration.
    pub bluesky: Option<BlueskyAnnounce>,
    /// LinkedIn announcement configuration.
    pub linkedin: Option<LinkedInAnnounce>,
    /// OpenCollective announcement configuration.
    pub opencollective: Option<OpenCollectiveAnnounce>,
}

impl AnnounceConfig {
    /// Resolve the overall announce-stage deadline, falling back to
    /// [`DEFAULT_ANNOUNCE_DEADLINE`] when `deadline:` is unset.
    ///
    /// An explicit but degenerate value (`"0s"` or anything below
    /// [`MIN_ANNOUNCE_DEADLINE`]) is clamped UP to the minimum: a zero/sub-second
    /// deadline would make the runner abandon every channel on the first tick
    /// (traffic leaves, every result warned as "did not complete"), so it is
    /// raised to the smallest budget under which a reachable channel can report.
    pub fn deadline_duration(&self) -> std::time::Duration {
        self.deadline
            .map(|d| d.duration())
            .unwrap_or(DEFAULT_ANNOUNCE_DEADLINE)
            .max(MIN_ANNOUNCE_DEADLINE)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BlueskyAnnounce {
    /// Enable Bluesky announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Bluesky handle/username (e.g. "user.bsky.social").
    pub username: Option<String>,
    /// Message template for the post. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Override the Bluesky PDS (Personal Data Server) URL. Defaults to
    /// `https://bsky.social`. Set this to point at a self-hosted PDS or
    /// alternative instance (e.g. `https://pds.example.com`).
    pub pds_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiscourseAnnounce {
    /// Enable Discourse announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Discourse forum URL (e.g. `https://forum.example.com`).
    pub server: Option<String>,
    /// Category ID to post in (required, must be non-zero).
    pub category_id: Option<u64>,
    /// Username for the API request (default: "system").
    pub username: Option<String>,
    /// Title template for the forum topic. Default: "{{ ProjectName }} {{ Tag }} is out!"
    pub title_template: Option<String>,
    /// Message body template for the forum topic. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LinkedInAnnounce {
    /// Enable LinkedIn announcements. Requires LINKEDIN_ACCESS_TOKEN env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Message template for the LinkedIn share post. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OpenCollectiveAnnounce {
    /// Enable OpenCollective announcements. Requires OPENCOLLECTIVE_TOKEN env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Collective slug (e.g. "my-project").
    pub slug: Option<String>,
    /// Title template for the update. Default: "{{ Tag }}"
    pub title_template: Option<String>,
    /// HTML message template for the update. Default includes `<br/>` and `<a>` tags with ReleaseURL.
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TwitterAnnounce {
    /// Enable Twitter/X announcements. Requires TWITTER_CONSUMER_KEY, TWITTER_CONSUMER_SECRET, TWITTER_ACCESS_TOKEN, TWITTER_ACCESS_TOKEN_SECRET env vars (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Tweet message template. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MastodonAnnounce {
    /// Enable Mastodon announcements. Requires `MASTODON_ACCESS_TOKEN` env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Mastodon instance URL (e.g. `https://mastodon.social`).
    pub server: Option<String>,
    /// Toot message template. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordAnnounce {
    /// Enable Discord announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Discord webhook URL.
    ///
    /// Prefer `{{ Env.DISCORD_WEBHOOK }}` (or similar) over an in-config
    /// literal — plaintext webhook URLs grant full posting access and are
    /// NOT redacted from error messages or `dist/config.yaml` after a
    /// dry-run / snapshot run.
    pub webhook_url: Option<String>,
    /// Message template for the Discord embed. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Author name displayed in the embed. Default: `anodizer`.
    pub author: Option<String>,
    /// Embed color as a decimal integer string (default: "3888754", a blue).
    /// Parsed to u32 at runtime. Supports template expressions.
    pub color: Option<String>,
    /// Icon URL shown beside the embed author; sent only when `author`
    /// renders to a name.
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    /// Enable generic webhook announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Webhook endpoint URL (supports template variables).
    ///
    /// Prefer `{{ Env.WEBHOOK_URL }}` for any URL containing a secret
    /// token in its path / query string — plaintext values are NOT
    /// redacted from error messages or `dist/config.yaml` after a
    /// dry-run / snapshot run.
    pub endpoint_url: Option<String>,
    /// Custom HTTP headers to include in the request.
    ///
    /// Precedence — **anodizer-specific**:
    /// - anodizer: a config-supplied `Authorization` header wins over the
    ///   `BASIC_AUTH_HEADER_VALUE` / `BEARER_TOKEN_HEADER_VALUE` env var.
    /// - The conventional behaviour: env-supplied `Authorization` is
    ///   appended FIRST; most servers honour the first occurrence, so the
    ///   env value effectively wins.
    ///
    /// Migrating configs that relied on env-overriding the config header
    /// must either remove the config entry or be reconfigured. Use
    /// templated config (`Authorization: "Bearer {{ Env.MY_TOKEN }}"`) for
    /// the cleanest migration.
    pub headers: Option<HashMap<String, String>>,
    /// Content-Type header value. Default: `application/json; charset=utf-8`.
    pub content_type: Option<String>,
    /// Message body template. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// When true, skip TLS certificate verification for the webhook endpoint.
    pub skip_tls_verify: Option<bool>,
    /// HTTP status codes to accept as success (default: [200, 201, 202, 204]).
    #[serde(default)]
    pub expected_status_codes: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramAnnounce {
    /// Enable Telegram announcements. Requires bot_token and chat_id (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Telegram Bot API token. Get one from @BotFather.
    ///
    /// Prefer `{{ Env.TELEGRAM_BOT_TOKEN }}` over an in-config literal —
    /// plaintext tokens grant full bot impersonation and are NOT redacted
    /// from error messages or `dist/config.yaml` after a dry-run / snapshot
    /// run.
    pub bot_token: Option<String>,
    /// Telegram chat ID to send the message to (supports template variables).
    pub chat_id: Option<String>,
    /// Message template. Default: "{{ ProjectName | mdv2escape }} {{ Tag | mdv2escape }} is out\\! Check it out at {{ ReleaseURL | mdv2escape }}"
    pub message_template: Option<String>,
    /// Parse mode: "MarkdownV2" or "HTML" (defaults to "MarkdownV2").
    pub parse_mode: Option<String>,
    /// Message thread ID for sending to a specific topic in a forum group.
    /// Supports template expressions; parsed to i64 at runtime.
    pub message_thread_id: Option<String>,
}

/// Default Adaptive Card title for Teams announcements. Centralised so that a
/// config-load round-trip (parse → serialise → re-parse) preserves the value
/// and does not strip it back to `None`.
pub const TEAMS_DEFAULT_TITLE_TEMPLATE: &str = "{{ ProjectName }} {{ Tag }} is out!";

fn default_teams_title_template() -> Option<String> {
    Some(TEAMS_DEFAULT_TITLE_TEMPLATE.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TeamsAnnounce {
    /// Enable Microsoft Teams announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Teams incoming webhook URL.
    pub webhook_url: Option<String>,
    /// Message template for the Adaptive Card body. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Title template for the Adaptive Card header. Default: "{{ ProjectName }} {{ Tag }} is out!"
    #[serde(default = "default_teams_title_template")]
    pub title_template: Option<String>,
    /// Theme color for the card (hex string, e.g. "0076D7").
    pub color: Option<String>,
    /// Icon URL displayed in the card header.
    pub icon_url: Option<String>,
}

impl Default for TeamsAnnounce {
    fn default() -> Self {
        Self {
            enabled: None,
            webhook_url: None,
            message_template: None,
            title_template: default_teams_title_template(),
            color: None,
            icon_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MattermostAnnounce {
    /// Enable Mattermost announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Mattermost incoming webhook URL.
    pub webhook_url: Option<String>,
    /// Channel override (e.g. "town-square").
    pub channel: Option<String>,
    /// Username override for the bot post.
    pub username: Option<String>,
    /// Icon URL for the bot post.
    pub icon_url: Option<String>,
    /// Icon emoji for the bot post (e.g. ":rocket:").
    pub icon_emoji: Option<String>,
    /// Attachment color (hex string, e.g. "#36a64f").
    pub color: Option<String>,
    /// Message template for the Mattermost post. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Title template for the Mattermost attachment.
    pub title_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EmailAnnounce {
    /// Enable email announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// SMTP server hostname. When set, uses SMTP transport.
    /// When absent, falls back to sendmail/msmtp.
    pub host: Option<String>,
    /// SMTP server port (default: 587 for STARTTLS).
    ///
    /// Anodizer-additive UX win (locked 2026-04-28): an unset SMTP
    /// `port` would otherwise be an error when it is
    /// unset (zero value). Anodizer defaults to 587 — the IETF submission
    /// port — so the common case (corporate / SaaS SMTP relays exposing
    /// STARTTLS on 587) works out of the box without a config knob. The
    /// `auto` encryption mode then resolves to STARTTLS for 587, which is
    /// the conventional pairing. Tested by
    /// `test_email_smtp_port_defaults_to_587`.
    pub port: Option<u16>,
    /// SMTP username (can also be set via SMTP_USERNAME env var).
    pub username: Option<String>,
    /// Sender email address.
    pub from: Option<String>,
    /// Recipient email addresses.
    #[serde(default)]
    pub to: Vec<String>,
    /// Email subject template. Default: "{{ ProjectName }} {{ Tag }} is out!"
    pub subject_template: Option<String>,
    /// Email body template.
    pub message_template: Option<String>,
    /// Skip TLS certificate verification (default: false).
    pub insecure_skip_verify: Option<bool>,
    /// Transport encryption mode. `auto` (the default) picks SMTPS for port
    /// 465, plain SMTP for port 25, and STARTTLS for everything else; `tls`
    /// forces SMTPS, `starttls` forces STARTTLS, `none` forces plain SMTP.
    pub encryption: Option<EmailEncryption>,
}

/// Email transport encryption mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmailEncryption {
    /// Pick based on port: 465 → SMTPS, 25 → none, otherwise STARTTLS.
    #[default]
    Auto,
    /// Implicit TLS on connect (typically port 465).
    Tls,
    /// Plain SMTP that upgrades to TLS via STARTTLS (typically port 587).
    Starttls,
    /// Plain SMTP, no TLS. Only safe on trusted local relays (port 25).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RedditAnnounce {
    /// Enable Reddit announcements. Requires REDDIT_SECRET and REDDIT_PASSWORD env vars (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Reddit application (OAuth client) ID.
    pub application_id: Option<String>,
    /// Reddit username for posting.
    pub username: Option<String>,
    /// Subreddit to post to (without /r/ prefix).
    pub sub: Option<String>,
    /// Title template for the Reddit link post. Default: "{{ ProjectName }} {{ Tag }} is out!"
    pub title_template: Option<String>,
    /// URL template for the Reddit link post. Default: "{{ ReleaseURL }}"
    pub url_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SlackAnnounce {
    /// Enable Slack announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Slack incoming webhook URL. Use template `{{ Env.SLACK_WEBHOOK }}` to reference an environment variable.
    pub webhook_url: Option<String>,
    /// Message template for the Slack post. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Override the webhook's default channel (e.g. "#releases").
    pub channel: Option<String>,
    /// Username the message is posted as (e.g. "release-bot"). Default:
    /// `anodizer`.
    pub username: Option<String>,
    /// Override the webhook's default icon with an emoji (e.g. ":rocket:").
    pub icon_emoji: Option<String>,
    /// Override the webhook's default icon with an image URL.
    pub icon_url: Option<String>,
    /// Slack Block Kit blocks (typed for schema validation).
    pub blocks: Option<Vec<SlackBlock>>,
    /// Slack legacy attachments (typed for schema validation).
    pub attachments: Option<Vec<SlackAttachment>>,
}

/// A Slack Block Kit block element.
/// Common fields are typed; additional block-type-specific fields are captured via flatten.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SlackBlock {
    /// Block type (e.g., "header", "section", "divider", "actions", "context", "image").
    #[serde(rename = "type")]
    pub block_type: String,
    /// Text object for the block (used by header, section, context types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<SlackTextObject>,
    /// Block ID for interactive payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// Additional block-specific fields (elements, accessory, fields, etc.).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A Slack text composition object.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlackTextObject {
    /// Text type: "plain_text" or "mrkdwn".
    #[serde(rename = "type")]
    pub text_type: String,
    /// Text content (supports template variables).
    pub text: String,
    /// Whether to render emoji shortcodes (plain_text only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<bool>,
    /// Whether to render verbatim (mrkdwn only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbatim: Option<bool>,
}

/// A Slack legacy attachment.
/// Common fields are typed; additional fields are captured via flatten.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SlackAttachment {
    /// Attachment sidebar color (hex string, e.g., "#36a64f" for green).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Main body text of the attachment (supports template variables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Bold title text at the top of the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Plain-text summary shown in notifications that cannot render attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// Text shown above the attachment block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pretext: Option<String>,
    /// Small text shown at the bottom of the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// Additional attachment-specific fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HumanDuration;
    use std::time::Duration;

    /// Every announcer's config field has a row in that announcer's docs page.
    /// The page tables are hand-written, so a field added to a struct without
    /// a row went undocumented until a reader asked where it was.
    #[test]
    fn every_announcer_field_has_a_row_in_its_docs_page() {
        let schema = serde_json::to_value(schemars::schema_for!(AnnounceConfig)).unwrap();
        let defs = &schema["$defs"];
        // The `webhook:` block is documented on `webhooks.md`.
        let page_of = |announcer: &str| match announcer {
            "webhook" => "webhooks".to_string(),
            other => other.to_string(),
        };
        let mut missing = Vec::new();
        let mut checked = 0usize;
        let mut announcers = 0usize;
        for (announcer, prop) in schema["properties"].as_object().unwrap() {
            let Some(def) = prop["anyOf"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::once(prop))
                .find_map(|v| v["$ref"].as_str())
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .filter(|d| d.ends_with("Announce") || *d == "WebhookConfig")
            else {
                continue;
            };
            announcers += 1;
            let page = crate::test_helpers::docs_pages::read_docs_page(&format!(
                "announce/{}.md",
                page_of(announcer)
            ));
            for field in defs[def]["properties"].as_object().unwrap().keys() {
                checked += 1;
                if !page.contains(&format!("| `{field}` |")) {
                    missing.push(format!("{}.md: `{field}`", page_of(announcer)));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "undocumented announcer fields:\n{}",
            missing.join("\n")
        );
        // Every property of AnnounceConfig but `skip`, `if`, `gate_on` and
        // `deadline` is an announcer; a new one whose type name is neither
        // `*Announce` nor the one `WebhookConfig` would be skipped silently
        // otherwise.
        let properties = schema["properties"].as_object().unwrap().len();
        assert_eq!(
            announcers,
            properties - 4,
            "an announcer was skipped by the scan"
        );
        assert_eq!(
            checked, 80,
            "the schema scan found a different number of fields"
        );
    }

    #[test]
    fn slack_blocks_and_attachments_survive_the_effective_config_round_trip() {
        let yaml = r##"
project_name: app
announce:
  slack:
    enabled: true
    blocks:
      - type: section
        text:
          type: plain_text
          text: 'Release "stable"'
        block_id: intro
        accessory:
          type: image
          image_url: https://example.invalid/a.png
      - type: section
        text:
          type: mrkdwn
          text: "{{ ReleaseNotes }}"
    attachments:
      - color: "#36a64f"
        text: 'Release "stable"'
        fields:
          - title: Version
            short: true
"##;
        let loaded: crate::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let effective = serde_yaml_ng::to_string(&loaded).unwrap();
        let reloaded: crate::config::Config = serde_yaml_ng::from_str(&effective).unwrap();
        let before = loaded
            .announce
            .as_ref()
            .and_then(|a| a.slack.as_ref())
            .unwrap();
        let after = reloaded
            .announce
            .as_ref()
            .and_then(|a| a.slack.as_ref())
            .unwrap();
        assert_eq!(
            serde_json::to_value(&before.blocks).unwrap(),
            serde_json::to_value(&after.blocks).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&before.attachments).unwrap(),
            serde_json::to_value(&after.attachments).unwrap()
        );
        let blocks = after.blocks.as_ref().unwrap();
        assert_eq!(blocks[0].text.as_ref().unwrap().text, "Release \"stable\"");
        assert_eq!(
            blocks[0].extra["accessory"]["image_url"],
            "https://example.invalid/a.png"
        );
        assert_eq!(blocks[1].text.as_ref().unwrap().text, "{{ ReleaseNotes }}");
        let attachments = after.attachments.as_ref().unwrap();
        assert_eq!(attachments[0].text.as_deref(), Some("Release \"stable\""));
        assert_eq!(attachments[0].extra["fields"][0]["short"], true);
    }

    /// Every announcer's `enabled` is an `Option<StringOrBool>`, so every
    /// announce page's field table says so; a page typing it `bool` tells
    /// the reader a template there is a config error.
    #[test]
    fn every_announce_page_types_enabled_as_a_template() {
        use crate::test_helpers::docs_pages::{docs_pages, read_docs_page};
        let mut pages = 0;
        for path in docs_pages() {
            if path.parent() != Some(std::path::Path::new("announce"))
                || path.file_name().is_some_and(|n| n == "_index.md")
            {
                continue;
            }
            let text = read_docs_page(path.to_str().expect("a UTF-8 page path"));
            let row = text
                .lines()
                .find(|l| l.starts_with("| `enabled` |"))
                .unwrap_or_else(|| panic!("{} has no `enabled` row", path.display()));
            assert!(
                row.starts_with("| `enabled` | bool or template |"),
                "{}: {row}",
                path.display()
            );
            pages += 1;
        }
        assert_eq!(pages, 14, "one page per announcer");
    }

    #[test]
    fn deadline_unset_uses_default() {
        let cfg = AnnounceConfig::default();
        assert_eq!(cfg.deadline_duration(), DEFAULT_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_zero_is_clamped_to_minimum() {
        // An explicit `deadline: "0s"` must NOT abandon every channel on the
        // first tick — it is raised to the 1s minimum.
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::ZERO)),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), MIN_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_subsecond_is_clamped_to_minimum() {
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::from_millis(250))),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), MIN_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_above_minimum_is_preserved() {
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::from_secs(45))),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), Duration::from_secs(45));
    }
}
