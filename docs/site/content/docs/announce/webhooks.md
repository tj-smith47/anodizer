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
    endpoint_url: "https://api.example.com/releases"
    content_type: "application/json"
    message_template: |
      {"project": "{{ ProjectName }}", "version": "{{ Version }}", "url": "{{ ReleaseURL }}"}
```

Use webhooks to integrate with any service that accepts HTTP POST requests. The message template can produce JSON, plain text, or any format your endpoint expects.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable webhook notifications |
| `endpoint_url` | string | Endpoint URL |
| `headers` | map | Custom HTTP headers |
| `content_type` | string | Content-Type header (e.g., `application/json`) |
| `message_template` | string | POST body (templates supported) |
