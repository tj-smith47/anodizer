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

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Discord notifications |
| `webhook_url` | string | Discord webhook URL (use template to read from env) |
| `message_template` | string | Message body (templates supported) |
