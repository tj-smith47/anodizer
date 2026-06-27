+++
title = "Gemfury"
description = "Push packages to Gemfury (fury.io)"
weight = 84
template = "docs.html"
+++

Anodizer can push deb, rpm, and apk packages to [Gemfury](https://fury.io/) repositories.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | true | per-version `DELETE` via the Fury API | `FURY_TOKEN` push + `FURY_API_TOKEN` delete |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
gemfury:
  - account: myorg
```

`FURY_PUSH_TOKEN` must be exported in the publish environment (or set
`gemfury[].token` to a templated value). The token is sent as the HTTP
Basic auth username (empty password) — the conventional Fury push
surface.

## Deprecation: `furies:` → `gemfury:`

GoReleaser Pro v2.14 renamed the top-level key from `furies:` to
`gemfury:`. Anodizer accepts both spellings via a serde alias and emits
a one-time deprecation warning when the legacy spelling is detected:

```text
DEPRECATION: the top-level `furies:` config key is deprecated since GoReleaser
Pro v2.14; rename it to `gemfury:`. Both spellings are accepted but the legacy
key will be removed in a future release.
```

## Full config reference

```yaml
gemfury:
  - id: primary                       # optional; selector for --id=...
    account: myorg                    # required; Gemfury account (templated)
    ids: [demo]                       # optional; filter by build IDs
    exclude: []                       # optional; drop packages whose name matches a glob
    formats: [deb, rpm, apk]          # optional; default ["apk","deb","rpm"]
    secret_name: FURY_PUSH_TOKEN      # optional; env var for the push token
    api_secret_name: FURY_API_TOKEN   # optional; env var for the delete token
    token: "{{ Env.MY_FURY_PUSH }}"  # optional; cfg-level templated push token
    api_token: "{{ Env.MY_FURY_API }}" # optional; cfg-level templated API token
    skip: false                       # optional; skip this entry (bool/template)
    required: true                    # optional; override required-default
    if: "{{ Prerelease != \"\" }}"    # optional; template-conditional gate
```

## Excluding sidecars with `exclude`

`exclude` is a list of globs matched against each artifact's **file name**;
anodizer drops every package whose name matches at least one glob from **this
GemFury account only**. Use it to keep heavy sidecars (checksums, signatures,
SBOMs) off the account while `.deb` / `.rpm` / `.apk` packages still upload.

```yaml
gemfury:
  - account: my-account
    exclude:
      - "*.sha256"
      - "*.sig"
      - "*.cdx.json"
```

`exclude` composes with `ids:` and the `formats:` filter — a package uploads
only when it passes every filter. An empty or unset `exclude` keeps everything.
Globs are validated at config-load; an `exclude` that drops every candidate
raises a warning so a typo'd glob is never a silent empty upload.

## Authentication

| Variable | Description |
|----------|-------------|
| `FURY_PUSH_TOKEN` | Push token, sent as HTTP Basic auth username. Override the env-var name via `secret_name`. |
| `FURY_API_TOKEN` | API token for the rollback `DELETE` endpoint. Override the env-var name via `api_secret_name`. |

The push and API tokens are usually different on Fury (push tokens are
repo-scoped; API tokens have account-wide privileges). Anodizer keeps
them separate so a workflow can grant push-only credentials by default
and only require the wider API token in the rollback flow.

## Behavior

- Pushes matching `linux_package` and `archive` artifacts via `POST https://push.fury.io/<account>` with multipart `package=<bytes>`.
- Authenticates with HTTP Basic auth (token as username, empty password).
- Format detection is by file extension: `.deb`, `.rpm`, `.apk`.
- Idempotency probe: `GET https://api.fury.io/<account>/packages/<name>/versions/<version>` before push; if already present, the push is skipped (matches the immutable-releases policy).
- Multi-format archive guard: when an `archives:` block declares
  multiple formats AND more than one extension lands in the configured
  gemfury formats filter, the publisher hard-errors with the offending
  crate name + format list so the operator narrows `formats:`.
- Retry: transient `5xx`/`429` failures retry with exponential backoff
  per the top-level `retry:` block. Non-transient failures break out
  immediately.
- Rollback: per uploaded artifact, issue `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`. When the API token is unavailable, rollback emits a manual-cleanup warn instead of erroring.

## Common gotchas

- Push tokens cannot delete — set `FURY_API_TOKEN` (or `gemfury[].api_token`) if you want rollback to fire programmatically.
- `formats:` matches by filename extension; multi-format archives need a narrowed filter to avoid pushing both `.deb` and `.rpm` (or set the corresponding archive block's `formats:` to one entry).

## Gemfury config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | CLI selector (`--id=...`) |
| `account` | string | **required** | Gemfury account name (templated) |
| `ids` | list | none | Build-ID filter |
| `formats` | list | `["apk", "deb", "rpm"]` | Package format filter |
| `secret_name` | string | `FURY_PUSH_TOKEN` | Env var name for the push token |
| `api_secret_name` | string | `FURY_API_TOKEN` | Env var name for the delete token |
| `token` | string | none | Cfg-supplied push token (templated) |
| `api_token` | string | none | Cfg-supplied API/delete token (templated) |
| `skip` | string/bool | none | Skip this entry (legacy `disable:` spelling accepted as an alias) |
| `required` | bool | `true` | Override required-default |
| `if` | string | none | Template-conditional gate |

## Full example

```yaml
gemfury:
  - account: myorg
    formats:
      - deb
      - rpm
    secret_name: MY_FURY_TOKEN
    api_secret_name: MY_FURY_API_TOKEN
```
