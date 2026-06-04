+++
title = "LinkedIn"
description = "Post release announcements to LinkedIn"
weight = 48
template = "docs.html"
+++

## Config

```yaml
announce:
  linkedin:
    enabled: true
    message_template: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable LinkedIn announcements |
| `message_template` | string | Share post text (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `LINKEDIN_ACCESS_TOKEN` | Yes | OAuth 2.0 access token for the posting account |

## How it works

Anodizer posts to LinkedIn via the v2 Share API. Each run:

1. Resolves the profile URN by calling `GET /v2/userinfo` (newer endpoint,
   reads the `sub` field). If that endpoint returns 403 Forbidden, it falls
   back to `GET /v2/me` (legacy endpoint, reads the `id` field) for
   compatibility with older app permission grants.
2. Posts a share via `POST /v2/shares` with the resolved URN as `owner`.
3. Logs the resulting activity URL
   (`https://www.linkedin.com/feed/update/<activity>`) to standard error.

## Obtaining an access token

LinkedIn access tokens are issued through OAuth 2.0. The steps depend on
whether you are posting as a personal profile or a company page:

**Personal profile:**

1. Create a LinkedIn app at <https://developer.linkedin.com>.
2. Add the `w_member_social` product (Share on LinkedIn).
3. Complete the OAuth 2.0 authorization code flow to obtain a token with the
   `w_member_social` scope.

Access tokens expire. For automated CI use, generate a long-lived token or
plan for token refresh.

## Example

```yaml
announce:
  linkedin:
    enabled: true
    message_template: |
      {{ ProjectName }} {{ Tag }} is out!

      Release notes: {{ ReleaseURL }}
```

```
LINKEDIN_ACCESS_TOKEN=AQV...your_token_here
```
