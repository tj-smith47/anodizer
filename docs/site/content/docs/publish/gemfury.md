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
| Assets | false | warn-only (no programmatic delete API; manual via Gemfury UI) | `FURY_TOKEN` push |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
fury:
  - account: myorg
```

## Full config reference

```yaml
fury:
  - account: myorg                   # required; Gemfury account name (template)
    ids: []                          # optional; filter by build IDs
    formats:                         # optional; defaults to ["apk", "deb", "rpm"]
      - deb
      - rpm
      - apk
    secret_name: FURY_TOKEN          # optional; env var name for the push token
    disable: false                   # optional; skip this entry
```

## Authentication

| Variable | Description |
|----------|-------------|
| `FURY_TOKEN` | Gemfury push token (or custom name via `secret_name`) |

## Common gotchas

- Gemfury's push endpoint (`https://push.fury.io/v1/{account}/`) accepts packages by file extension. Ensure `formats` matches the package types your build actually produces.
- Format detection is by file extension: `.apk` maps to `alpine` in the format filter list.
- Gemfury has no programmatic delete API; rollback is warn-only. Use the Gemfury web UI to remove a version if needed.

## Republish / update behavior

Not applicable as a config field — Gemfury allows re-pushing the same version file (the PUT endpoint overwrites). No flag is required. Rollback is warn-only because Gemfury has no delete API in anodizer's scope.

## Gemfury config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `account` | string | **required** | Gemfury account name (template) |
| `ids` | list | none | Filter by build IDs |
| `formats` | list | `["apk", "deb", "rpm"]` | Package format filter |
| `secret_name` | string | `FURY_TOKEN` | Environment variable name for the API token |
| `disable` | string/bool | none | Disable this config |

## Behavior

- Pushes matching `linux_package` and `archive` artifacts via HTTP PUT to `https://push.fury.io/v1/{account}/`
- Authenticates with Bearer token
- Matches artifacts by file extension against the format filter
- Supports multiple entries and ID filtering

## Full example

```yaml
fury:
  - account: myorg
    formats:
      - deb
      - rpm
    secret_name: MY_FURY_TOKEN
```
