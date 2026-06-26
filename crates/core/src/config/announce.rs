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
/// this deadline are abandoned with a warning, never awaited forever.
pub const DEFAULT_ANNOUNCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Floor for the resolved announce deadline.
///
/// An explicit `deadline: "0s"` (or any sub-second value) would let the runner
/// abandon every channel on its first scheduling tick — traffic still leaves,
/// but every result is warned as "did not complete", which is a useless and
/// surprising state. Clamping the resolved deadline up to this floor makes that
/// footgun unrepresentable: the smallest meaningful aggregate budget is one
/// second, enough for a reachable local relay to report.
pub const MIN_ANNOUNCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------

/// Announce-stage gate semantics.
///
/// Decides whether [`AnnounceStage`] runs based on the `PublishReport`
/// produced by `PublishStage` (and contributed to by `BlobStage`):
///
/// - `required_publishers` (default): announce runs only if every
///   `required: true` publisher across the run succeeded.
/// - `all_publishers`: announce runs only if every configured
///   publisher succeeded (Submitter gate failures count here too).
/// - `none`: announce always runs.
///
/// [`AnnounceStage`]: ../../stage-announce/struct.AnnounceStage.html
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
    /// Template-conditional skip: if rendered to "true", skip the entire announce stage.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the entire announce stage is
    /// skipped. Render failure hard-errors. The
    /// `announce.if:`. Distinct from `skip:` (always-skip predicate) — both
    /// surfaces are documented.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Selects when AnnounceStage runs vs. skips based on the
    /// `PublishReport` written by PublishStage/BlobStage. Default is
    /// `required_publishers` (announce only if every required publisher
    /// succeeded). See [`AnnounceGate`] for the other variants.
    #[serde(default)]
    pub gate_on: AnnounceGate,
    /// Overall wall-clock deadline for the announce stage (e.g. `"90s"`,
    /// `"2m"`). Optional — defaults to [`DEFAULT_ANNOUNCE_DEADLINE`] (90s).
    ///
    /// Announcers run concurrently; any still running when this deadline
    /// elapses is abandoned with a warning rather than awaited. This bounds the
    /// stage so unreachable channels cannot accumulate into a hang that trips
    /// the pipeline timeout *after* publishers already crossed one-way doors.
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
    /// [`MIN_ANNOUNCE_DEADLINE`]) is clamped UP to the floor: a zero/sub-second
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
    /// Discourse forum URL (e.g. "https://forum.example.com").
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
    /// HTML message template for the update. Default includes <br/> and <a> tags with ReleaseURL.
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
    /// Mastodon instance URL (e.g. "https://mastodon.social").
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
    /// Author name displayed in the embed.
    pub author: Option<String>,
    /// Embed color as a decimal integer string (default: "3888754", a blue).
    /// Parsed to u32 at runtime. Supports template expressions.
    pub color: Option<String>,
    /// Icon URL for the embed footer.
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
    /// Content-Type header value. Default: "application/json".
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
    /// Message template. Default: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
    pub message_template: Option<String>,
    /// Parse mode: "MarkdownV2" or "HTML" (defaults to "MarkdownV2").
    pub parse_mode: Option<String>,
    /// Message thread ID for sending to a specific topic in a forum group.
    /// Supports template expressions; parsed to i64 at runtime.
    pub message_thread_id: Option<String>,
}

/// Default Adaptive Card title for Teams announcements. Centralised so that a
/// config-load round-trip (parse → serialise → re-parse) preserves the value
/// instead of stripping it back to `None`.
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
    /// Anodize-additive UX win (locked 2026-04-28): an unset SMTP
    /// `port` would otherwise be an error when it is
    /// unset (zero value). Anodize defaults to 587 — the IETF submission
    /// port — so the common case (corporate / SaaS SMTP relays exposing
    /// STARTTLS on 587) works out of the box without a config knob. The
    /// `auto` encryption mode then resolves to STARTTLS for 587, which is
    /// the conventional pairing. Pinned by
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
    /// Override the webhook's default username (e.g. "release-bot").
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

    #[test]
    fn deadline_unset_uses_default() {
        let cfg = AnnounceConfig::default();
        assert_eq!(cfg.deadline_duration(), DEFAULT_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_zero_is_clamped_to_floor() {
        // An explicit `deadline: "0s"` must NOT abandon every channel on the
        // first tick — it is raised to the 1s floor.
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::ZERO)),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), MIN_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_subsecond_is_clamped_to_floor() {
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::from_millis(250))),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), MIN_ANNOUNCE_DEADLINE);
    }

    #[test]
    fn deadline_above_floor_is_preserved() {
        let cfg = AnnounceConfig {
            deadline: Some(HumanDuration(Duration::from_secs(45))),
            ..Default::default()
        };
        assert_eq!(cfg.deadline_duration(), Duration::from_secs(45));
    }
}
