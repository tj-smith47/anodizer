+++
title = "Mattermost"
description = "Send release notifications to Mattermost"
weight = 6
template = "docs.html"
+++

## Config

```yaml
announce:
  mattermost:
    enabled: true
    webhook_url: "{{ Env.MATTERMOST_WEBHOOK_URL }}"
    channel: "releases"
    username: "release-bot"
    icon_url: "https://example.com/icon.png"
    message_template: "{{ ProjectName }} {{ Tag }} has been released! {{ ReleaseURL }}"
```

Create an incoming webhook in **Main Menu > Integrations > Incoming Webhooks**. The webhook payload format is the same as Slack's, so the setup should feel familiar if you have used the Slack provider.

### Optional overrides

The `channel`, `username`, and `icon_url` fields are all optional. When omitted, Mattermost uses the defaults configured on the webhook itself.

- **`channel`** -- Override the default channel (e.g. `town-square` or `releases`).
- **`username`** -- Override the display name of the bot post.
- **`icon_url`** -- Override the bot's avatar with a custom image URL.

### Environment variables

Store the webhook URL in an environment variable rather than committing it to your config file:

```yaml
webhook_url: "{{ Env.MATTERMOST_WEBHOOK_URL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Mattermost notifications |
| `webhook_url` | string | Mattermost incoming webhook URL (use template to read from env) |
| `channel` | string | Optional channel override |
| `username` | string | Optional display name override |
| `icon_url` | string | Optional bot avatar URL |
| `message_template` | string | Message body (templates supported) |
