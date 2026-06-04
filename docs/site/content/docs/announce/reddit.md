+++
title = "Reddit"
description = "Post release announcements to Reddit as link posts"
weight = 44
template = "docs.html"
+++

## Config

```yaml
announce:
  reddit:
    enabled: true
    application_id: "your_app_id"
    username: "your_reddit_username"
    sub: "rust"
    title_template: "{{ ProjectName }} {{ Tag }} is out!"
    url_template: "{{ ReleaseURL }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Reddit announcements |
| `application_id` | string | Reddit OAuth application (client) ID |
| `username` | string | Reddit account username for posting |
| `sub` | string | Subreddit name to post to (without the `/r/` prefix) |
| `title_template` | string | Title of the link post (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out!` |
| `url_template` | string | URL the link post points to (templates supported). Default: `{{ ReleaseURL }}` |

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `REDDIT_SECRET` | Yes | OAuth application secret for the Reddit app |
| `REDDIT_PASSWORD` | Yes | Password for the Reddit account specified in `username` |

## How it works

Reddit requires an OAuth2 bearer token even for script-type apps that use
password auth. Anodizer performs a two-step flow on every run:

1. Exchanges `application_id` + `REDDIT_SECRET` + `username` + `REDDIT_PASSWORD`
   for a short-lived bearer token via `POST /api/v1/access_token`.
2. Submits a link post to the target subreddit via
   `POST https://oauth.reddit.com/api/submit`.

Reddit's API returns HTTP 200 even when a submission fails at the application
level. Anodizer checks the `json.errors` array in the response body and treats
any non-empty errors array as a failure.

## Setting up a Reddit app

1. Go to <https://www.reddit.com/prefs/apps> and create a new **script** app.
2. Copy the **client ID** (shown under the app name) into `application_id`.
3. Copy the **secret** into `REDDIT_SECRET`.
4. Use the Reddit account that owns the app as `username` and set `REDDIT_PASSWORD`.

Script-type apps are limited to posting as the app owner. If you need to post
as a different account, create a separate Reddit account and app for that
account.

## Example with secrets from environment

```yaml
announce:
  reddit:
    enabled: true
    application_id: "{{ Env.REDDIT_APP_ID }}"
    username: "{{ Env.REDDIT_USERNAME }}"
    sub: "myproject"
    title_template: "{{ ProjectName }} {{ Tag }} released"
    url_template: "{{ ReleaseURL }}"
```

```
REDDIT_APP_ID=abc123xyz
REDDIT_USERNAME=release_bot
REDDIT_SECRET=super_secret_value
REDDIT_PASSWORD=bot_account_password
```
