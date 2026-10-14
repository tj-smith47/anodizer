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

### Delivery

With `host` set (or `SMTP_HOST` in the environment), anodizer connects to that SMTP server itself and logs in as `username` (or `SMTP_USERNAME`) with `SMTP_PASSWORD`. `host` and `username` are plain strings and are not rendered as templates; leave `username` out to take it from the environment. The password is required unless the connection is plain SMTP (`encryption: none`, or port 25 under `auto`):

```yaml
announce:
  email:
    enabled: true
    host: smtp.example.com
    from: releases@example.com
    to: [team@example.com]
```

Without a host, the RFC 2822 message is piped to a local mail transfer agent: `sendmail -t` first, then `msmtp -t` if sendmail is not found. Both read recipients from the message headers. Make sure one of these programs is installed and configured on the machine that runs anodizer (your CI runner, for example). For lightweight setups, [msmtp](https://marlam.de/msmtp/) works well with a simple `~/.msmtprc` pointing at your SMTP relay.

### Templates

Both `subject_template` and `message_template` support the standard template variables (`ProjectName`, `Tag`, `Version`, `ReleaseURL`, and so on). The subject line and message body are rendered independently.

### Header injection protection

All header values (From, To, Subject) are sanitized by collapsing any embedded CR or LF characters to spaces. This prevents header injection attacks where a crafted value could forge extra headers in the outgoing message.

### Multiple recipients

The `to` field accepts a list. Each address becomes a comma-separated entry in the `To:` header of the generated message.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | bool or template | Enable email notifications |
| `host` | string | SMTP server. When set (or `SMTP_HOST` is), the message is sent over SMTP; otherwise it is piped to sendmail/msmtp |
| `port` | int | SMTP port (or `SMTP_PORT`). Default: `587` |
| `username` | string | SMTP username (or `SMTP_USERNAME`). Required for SMTP delivery; the password comes from `SMTP_PASSWORD`, required unless the connection is plain SMTP |
| `insecure_skip_verify` | bool | Skip TLS certificate verification on the SMTP connection. Default: `false` |
| `encryption` | string | `auto`, `tls`, `starttls` or `none`. Default: `auto`: SMTPS on port 465, plain SMTP on 25, STARTTLS on every other port |
| `from` | string | Sender address |
| `to` | list of strings | Recipient addresses |
| `subject_template` | string | Subject line (templates supported). Default: `{{ ProjectName }} {{ Tag }} is out!` |
| `message_template` | string | Message body (templates supported). Default: `You can view details from: {{ ReleaseURL }}` |
