+++
title = "Microsoft Teams"
description = "Send release notifications to Microsoft Teams"
weight = 5
template = "docs.html"
+++

## Config

```yaml
announce:
  teams:
    enabled: true
    webhook_url: "{{ Env.TEAMS_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} has been released! {{ ReleaseURL }}"
```

Create an incoming webhook in your Teams channel via **Connectors > Incoming Webhook** and copy the generated URL.

### Adaptive Card format

The message is delivered as a Microsoft [Adaptive Card](https://adaptivecards.io/) (v1.4) wrapped inside a `message` payload. The `message_template` value becomes the text of a `TextBlock` element with word-wrap enabled. You do not need to construct the card JSON yourself -- anodize handles that.

### Environment variables

Store the webhook URL in an environment variable rather than committing it to your config file:

```yaml
webhook_url: "{{ Env.TEAMS_WEBHOOK_URL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Teams notifications |
| `webhook_url` | string | Teams incoming webhook URL (use template to read from env) |
| `message_template` | string | Message body (templates supported) |
