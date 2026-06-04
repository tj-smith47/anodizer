+++
title = "Discourse"
description = "Post release announcements to Discourse forums"
weight = 50
template = "docs.html"
+++

## Config

```yaml
announce:
  discourse:
    enabled: true
    server: "https://forum.example.com"
    category_id: 5
    username: "release-bot"
    title_template: "{{ ProjectName }} {{ Tag }} is out!"
    message_template: "{{ ProjectName }} {{ Tag }} has been released. See the full release notes at {{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Discourse announcements |
| `server` | string | Full URL of your Discourse instance (e.g. `https://forum.example.com`) |
| `category_id` | integer | Category ID to create the topic in (required, must be non-zero) |
| `username` | string | API username for the request. Default: `system` |
| `title_template` | string | Topic title (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out!` |
| `message_template` | string | Topic body in Markdown (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `DISCOURSE_API_KEY` | Yes | API key from your Discourse admin panel |

## How it works

Anodizer posts to `{server}/posts.json` using Discourse's API key authentication
(`Api-Key` and `Api-Username` headers). The post is created with `kind: topic`,
placing it in the specified category. Trailing slashes on `server` are
automatically stripped.

## Finding the category ID

1. Go to your Discourse admin panel.
2. Navigate to **Admin → Categories**.
3. Click on the target category; the ID appears in the URL
   (`/c/category-name/<ID>`).

Alternatively, visit `{server}/categories.json` to list all categories with
their IDs.

## Creating an API key

1. Go to **Admin → API → New API Key**.
2. Set **User Level** to **Single User** and choose the account that will
   post the announcements.
3. Set **Scope** to **Write** (or use a global key scoped to posts).
4. Copy the generated key into `DISCOURSE_API_KEY`.

## Example

```yaml
announce:
  discourse:
    enabled: true
    server: "https://forum.myproject.io"
    category_id: 12
    username: "bot"
    title_template: "{{ ProjectName }} {{ Tag }} released"
    message_template: |
      ## {{ ProjectName }} {{ Tag }}

      A new release is available. See full release notes at {{ ReleaseURL }}.
```

```
DISCOURSE_API_KEY=your_api_key_here
```
