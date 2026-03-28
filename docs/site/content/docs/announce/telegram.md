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
    bot_token: "{{ Env.TELEGRAM_BOT_TOKEN }}"
    chat_id: "-100123456789"
    parse_mode: "MarkdownV2"
    message_template: "{{ ProjectName }} {{ Tag }} has been released!"
```

Create a Telegram bot via [@BotFather](https://t.me/botfather) and add it to the target group or channel. The `chat_id` is the numeric ID of the group or channel (prefix with `-100` for supergroups/channels), or a public channel username like `@mychannel`.

### Parse mode

The optional `parse_mode` field controls how Telegram renders the message text. Supported values:

- **`MarkdownV2`** -- Telegram-flavored Markdown (requires escaping special characters).
- **`HTML`** -- a subset of HTML tags (`<b>`, `<i>`, `<a>`, `<code>`, `<pre>`).
- Omit the field to send plain text with no formatting.

### Environment variables

Store the bot token in an environment variable rather than committing it to your config file:

```yaml
bot_token: "{{ Env.TELEGRAM_BOT_TOKEN }}"
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable Telegram notifications |
| `bot_token` | string | Telegram Bot API token (use template to read from env) |
| `chat_id` | string | Target chat, group, or channel ID |
| `parse_mode` | string | Optional: `MarkdownV2` or `HTML` |
| `message_template` | string | Message body (templates supported) |
