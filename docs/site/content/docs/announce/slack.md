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
| `enabled` | bool or template | Enable Slack notifications |
| `webhook_url` | string | Slack webhook URL (falls back to `SLACK_WEBHOOK`) |
| `message_template` | string | Message body (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |
| `channel` | string | Override the webhook's default channel (`#releases`) |
| `username` | string | Username the post arrives under. Default: `anodizer` |
| `icon_emoji` | string | Override the webhook's default icon with an emoji (`:rocket:`) |
| `icon_url` | string | Override the webhook's default icon with an image URL |
| `blocks` | list | Slack Block Kit blocks; every string in them is a template |
| `attachments` | list | Slack legacy attachments; every string in them is a template |

## Blocks and attachments

```yaml
announce:
  slack:
    webhook_url: "{{ Env.SLACK_WEBHOOK }}"
    message_template: "{{ ProjectName }} {{ Tag }} is out"
    blocks:
      - type: header
        text:
          type: plain_text
          text: '{{ ProjectName }} {{ Tag }} "stable"'
      - type: section
        text:
          type: mrkdwn
          text: "{{ ReleaseNotes }}"
    attachments:
      - color: "#36a64f"
        title: "{{ Tag }}"
        text: "{{ ReleaseURL }}"
```

Each string is rendered on its own, so a literal quote in a block's text and
the newlines a `{{ ReleaseNotes }}` body produces reach Slack as written.
`message_template` is the fallback text Slack shows where blocks cannot be
rendered. Block-specific fields (`accessory`, `elements`, `fields`, …) are
passed through unchanged, and `dist/config.yaml` records them as written.
