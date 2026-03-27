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
    webhook_url: "{{ Env.SLACK_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} released: {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Slack notifications |
| `webhook_url` | string | Slack webhook URL |
| `message_template` | string | Message body (templates supported) |
