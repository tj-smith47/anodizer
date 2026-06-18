+++
title = "Discord"
description = "Send release notifications to Discord"
weight = 1
template = "docs.html"
+++

## Config

```yaml
announce:
  discord:
    enabled: true
    webhook_url: "{{ Env.DISCORD_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} has been released! {{ ReleaseURL }}"
```

When `webhook_url` is unset, anodizer assembles the webhook URL from the `DISCORD_WEBHOOK_ID` and `DISCORD_WEBHOOK_TOKEN` env var pair.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Discord notifications |
| `webhook_url` | string | Discord webhook URL (falls back to the `DISCORD_WEBHOOK_ID` + `DISCORD_WEBHOOK_TOKEN` env pair) |
| `message_template` | string | Message body (templates supported) |
