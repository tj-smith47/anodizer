+++
title = "Telegram"
description = "Send release notifications to Telegram"
weight = 4
template = "docs.html"
+++

## Config

```yaml
announce:
  telegram:
    enabled: true
    bot_token: "{{ Env.TELEGRAM_TOKEN }}"
    chat_id: "-100123456789"
    parse_mode: "MarkdownV2"
    message_template: "{{ ProjectName }} {{ Tag }} has been released!"
```

Create a Telegram bot via [@BotFather](https://t.me/botfather) and add it to the target group or channel. The `chat_id` is the numeric ID of the group or channel (prefix with `-100` for supergroups/channels), or a public channel username like `@mychannel`.

### Parse mode

The optional `parse_mode` field controls how Telegram renders the message text. Supported values:

- **`MarkdownV2`** (the default) -- Telegram-flavored Markdown. Pass every dynamic value through the `mdv2escape` filter and backslash-escape literal `!`, `.`, `-` and the other reserved characters, as the default template does.
- **`HTML`** -- a subset of HTML tags (`<b>`, `<i>`, `<a>`, `<code>`, `<pre>`).

### Environment variables

Store the bot token in an environment variable so it stays out of your committed config file. When `bot_token` is unset, anodizer falls back to the `TELEGRAM_TOKEN` env var:

```yaml
announce:
  telegram:
    bot_token: "{{ Env.TELEGRAM_TOKEN }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool or template | Enable Telegram notifications |
| `bot_token` | string | Telegram Bot API token (falls back to `TELEGRAM_TOKEN`) |
| `chat_id` | string | Target chat, group, or channel ID |
| `parse_mode` | string | `MarkdownV2` or `HTML`. Default: `MarkdownV2` |
| `message_template` | string | Message body (templates supported). Default: `{{ ProjectName | mdv2escape }} {{ Tag | mdv2escape }} is out\! Check it out at {{ ReleaseURL | mdv2escape }}` |
| `message_thread_id` | string | Topic (thread) to post in inside a forum group (templates supported; must render an integer) |
