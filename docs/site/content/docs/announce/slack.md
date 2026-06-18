+++
title = "Slack"
description = "Send release notifications to Slack"
weight = 2
template = "docs.html"
+++

## Config

```yaml
announce:
  slack:
    enabled: true
    webhook_url: "{{ Env.SLACK_WEBHOOK }}"
    message_template: "{{ ProjectName }} {{ Tag }} released: {{ ReleaseURL }}"
```

When `webhook_url` is unset, anodizer falls back to the `SLACK_WEBHOOK` env var.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Slack notifications |
| `webhook_url` | string | Slack webhook URL (falls back to `SLACK_WEBHOOK`) |
| `message_template` | string | Message body (templates supported) |
