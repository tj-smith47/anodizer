+++
title = "Twitter / X"
description = "Post release announcements to Twitter/X"
weight = 45
template = "docs.html"
+++

## Config

```yaml
announce:
  twitter:
    enabled: true
    message_template: "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Twitter/X announcements |
| `message_template` | string | Tweet text (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |

## Environment variables

All four OAuth 1.0a tokens are required.

| Variable | Required | Description |
|----------|----------|-------------|
| `TWITTER_CONSUMER_KEY` | Yes | API key (consumer key) for your Twitter app |
| `TWITTER_CONSUMER_SECRET` | Yes | API key secret (consumer secret) for your Twitter app |
| `TWITTER_ACCESS_TOKEN` | Yes | Access token for the posting account |
| `TWITTER_ACCESS_TOKEN_SECRET` | Yes | Access token secret for the posting account |

## Authentication

Anodizer uses OAuth 1.0a with HMAC-SHA1 signing to authenticate with the
Twitter API v2 (`POST https://api.x.com/2/tweets`). Each request gets a fresh
nonce and timestamp, so tokens can be long-lived without rotation.

To obtain tokens:

1. Create a project and app at <https://developer.x.com/en/portal/dashboard>.
2. Set the app permissions to **Read and Write**.
3. Under **Keys and Tokens**, generate an access token and secret **for your
   account** (not just the app-level keys).
4. Copy all four values into the corresponding environment variables.

## Tweet length

Twitter enforces a 280-character limit on tweets. If `message_template`
renders to more than 280 characters the API will return an error. Keep
templates concise or use a short release URL.

## Example

```yaml
announce:
  twitter:
    enabled: true
    message_template: "{{ ProjectName }} {{ Tag }} released — {{ ReleaseURL }}"
```

```
TWITTER_CONSUMER_KEY=your_api_key
TWITTER_CONSUMER_SECRET=your_api_key_secret
TWITTER_ACCESS_TOKEN=your_access_token
TWITTER_ACCESS_TOKEN_SECRET=your_access_token_secret
```
