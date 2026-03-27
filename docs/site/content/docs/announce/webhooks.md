+++
title = "Webhooks"
description = "Send release notifications to generic webhook endpoints"
weight = 3
template = "docs.html"
+++

## Config

```yaml
announce:
  webhook:
    enabled: true
    webhook_url: "https://api.example.com/releases"
    message_template: |
      {"project": "{{ ProjectName }}", "version": "{{ Version }}", "url": "{{ ReleaseURL }}"}
```

Use webhooks to integrate with any service that accepts HTTP POST requests. The message template can produce JSON, plain text, or any format your endpoint expects.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable webhook notifications |
| `webhook_url` | string | Endpoint URL |
| `message_template` | string | POST body (templates supported) |
