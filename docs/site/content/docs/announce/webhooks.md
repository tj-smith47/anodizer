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
| `enabled` | bool or template | Enable webhook notifications |
| `endpoint_url` | string | Endpoint URL |
| `headers` | map | Custom HTTP headers. A header named here replaces the default of the same name, including `Content-Type`, `User-Agent` and the `Authorization` header derived from `BASIC_AUTH_HEADER_VALUE` / `BEARER_TOKEN_HEADER_VALUE` |
| `content_type` | string | Content-Type header. Default: `application/json; charset=utf-8` |
| `message_template` | string | POST body (templates supported). Default: `{"message":"{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"}` |
| `skip_tls_verify` | bool | Skip TLS certificate verification for the endpoint. Default: `false` |
| `expected_status_codes` | list of ints | Response status codes accepted as success. Default: `[200, 201, 202, 204]` |
