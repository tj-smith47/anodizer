+++
title = "OpenCollective"
description = "Publish release updates to your OpenCollective collective"
weight = 49
template = "docs.html"
+++

## Config

```yaml
announce:
  opencollective:
    enabled: true
    slug: "my-project"
    title_template: "{{ Tag }}"
    message_template: "{{ ProjectName }} {{ Tag }} is out!<br/>Check it out at <a href=\"{{ ReleaseURL }}\">{{ ReleaseURL }}</a>"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable OpenCollective announcements |
| `slug` | string | The collective slug from your OpenCollective URL (e.g. `my-project` from `opencollective.com/my-project`) |
| `title_template` | string | Update title (templates supported). Default: `{{ Tag }}` |
| `message_template` | string | Update body as HTML (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out!<br/>Check it out at <a href="{{ ReleaseURL }}">{{ ReleaseURL }}</a>` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `OPENCOLLECTIVE_TOKEN` | Yes | Personal API token from your OpenCollective account |

## How it works

Anodizer uses the OpenCollective GraphQL v2 API at
`https://api.opencollective.com/graphql/v2` with your personal token in the
`Personal-Token` header. Each run performs a two-step flow:

1. Creates a draft update using the `createUpdate` mutation, associating it
   with your collective via the `slug` and providing the rendered title and
   HTML body.
2. Publishes the update immediately using the `publishUpdate` mutation with
   `notificationAudience: ALL`, sending email notifications to all collective
   backers and followers.

## Empty slug handling

If `slug` renders to an empty string, anodizer logs a warning and skips the
OpenCollective announcement without failing the pipeline.

## HTML in message body

The `message_template` is sent as HTML to OpenCollective. You can use standard
HTML tags (`<br/>`, `<a>`, `<strong>`, etc.) in your template. Plain text
without tags is also accepted.

## Obtaining a personal token

1. Log in to <https://opencollective.com>.
2. Go to **Settings → For Developers → Personal Tokens**.
3. Create a new token with at least **Account Updates** write access.
4. Copy the token into `OPENCOLLECTIVE_TOKEN`.

## Example

```yaml
announce:
  opencollective:
    enabled: true
    slug: "my-rust-project"
    title_template: "{{ ProjectName }} {{ Tag }} released"
    message_template: |
      <p>{{ ProjectName }} {{ Tag }} is now available.</p>
      <p>See the full release notes at <a href="{{ ReleaseURL }}">{{ ReleaseURL }}</a>.</p>
```

```
OPENCOLLECTIVE_TOKEN=your_personal_token_here
```
