+++
title = "Mastodon"
description = "Post release announcements to a Mastodon instance"
weight = 46
template = "docs.html"
+++

## Config

```yaml
announce:
  mastodon:
    enabled: true
    server: "https://mastodon.social"
    message_template: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Mastodon announcements |
| `server` | string | Full URL of your Mastodon instance (e.g. `https://mastodon.social`) |
| `message_template` | string | Toot text (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `MASTODON_ACCESS_TOKEN` | Yes | User access token for the posting account |
| `MASTODON_CLIENT_ID` | Yes | OAuth application client ID |
| `MASTODON_CLIENT_SECRET` | Yes | OAuth application client secret |

All three credentials are required and validated fail-fast: a missing
`MASTODON_CLIENT_ID` or `MASTODON_CLIENT_SECRET` aborts the release up front
rather than mid-announce. `MASTODON_ACCESS_TOKEN` is sent as a Bearer
credential on `POST /api/v1/statuses`; the client ID and secret back the
OAuth refresh flow.

## Empty server handling

If `server` renders to an empty string, anodizer logs a warning and skips the
Mastodon announcement without failing the pipeline.

## Obtaining credentials

1. Log in to your Mastodon instance.
2. Go to **Preferences → Development → New Application**.
3. Grant the `write:statuses` scope.
4. Copy the **Your access token** → `MASTODON_ACCESS_TOKEN`.

## Example

```yaml
announce:
  mastodon:
    enabled: true
    server: "https://fosstodon.org"
    message_template: "{{ ProjectName }} {{ Tag }} is out! {{ ReleaseURL }} #rustlang"
```

```
MASTODON_ACCESS_TOKEN=xyz789
```
