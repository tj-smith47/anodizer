+++
title = "Bluesky"
description = "Post release announcements to Bluesky via the AT Protocol"
weight = 47
template = "docs.html"
+++

## Config

```yaml
announce:
  bluesky:
    enabled: true
    username: "yourhandle.bsky.social"
    message_template: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Bluesky announcements |
| `username` | string | Bluesky handle (e.g. `yourhandle.bsky.social`) |
| `message_template` | string | Post text (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `BLUESKY_APP_PASSWORD` | Yes | App password generated from your Bluesky account settings |

## Authentication and AT Protocol

Anodizer uses the AT Protocol (`com.atproto`) to post to Bluesky. Each run
performs a two-step flow against `https://bsky.social`:

1. Creates a session via `POST /xrpc/com.atproto.server.createSession` using
   `username` and `BLUESKY_APP_PASSWORD` to receive an access JWT and the
   account's DID.
2. Creates a post record via `POST /xrpc/com.atproto.repo.createRecord` using
   the `app.bsky.feed.post` lexicon.

## Automatic link facets

If `{{ ReleaseURL }}` is present in the rendered message, anodizer
automatically adds a rich-text facet marking the URL as a clickable link.
Bluesky requires explicit facet annotations for links to be rendered as
hyperlinks rather than plain text, so this happens without any extra
configuration.

## App passwords

App passwords are separate credentials scoped to specific apps. They are
recommended over your main account password:

1. Go to **Settings → Privacy and Security → App Passwords**.
2. Click **Add App Password**, give it a name (e.g. `anodizer`).
3. Copy the generated password into `BLUESKY_APP_PASSWORD`.

## Example

```yaml
announce:
  bluesky:
    enabled: true
    username: "myrustproject.bsky.social"
    message_template: "{{ ProjectName }} {{ Tag }} released! {{ ReleaseURL }}"
```

```
BLUESKY_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx
```
