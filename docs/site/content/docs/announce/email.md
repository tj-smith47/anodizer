+++
title = "Email"
description = "Send release notifications via email"
weight = 7
template = "docs.html"
+++

## Config

```yaml
announce:
  email:
    enabled: true
    from: "release-bot@example.com"
    to:
      - "dev-team@example.com"
      - "ops@example.com"
    subject_template: "{{ ProjectName }} {{ Tag }} released"
    message_template: "{{ ProjectName }} {{ Tag }} has been released! {{ ReleaseURL }}"
```

### SMTP delivery

Email is sent by piping an RFC 2822 message to a local mail transfer agent. Anodize tries `sendmail -t` first and falls back to `msmtp -t` if sendmail is not found. Both read recipients from the message headers.

Make sure one of these programs is installed and configured on the machine that runs anodize (your CI runner, for example). For lightweight setups, [msmtp](https://marlam.de/msmtp/) works well with a simple `~/.msmtprc` pointing at your SMTP relay.

### Templates

Both `subject_template` and `message_template` support the standard template variables (`ProjectName`, `Tag`, `Version`, `ReleaseURL`, and so on). The subject line and message body are rendered independently.

### Header injection protection

All header values (From, To, Subject) are sanitized by collapsing any embedded CR or LF characters to spaces. This prevents header injection attacks where a crafted value could forge extra headers in the outgoing message.

### Multiple recipients

The `to` field accepts a list. Each address becomes a comma-separated entry in the `To:` header of the generated message.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool | Enable email notifications |
| `from` | string | Sender address |
| `to` | list of strings | Recipient addresses |
| `subject_template` | string | Subject line (templates supported) |
| `message_template` | string | Message body (templates supported) |
