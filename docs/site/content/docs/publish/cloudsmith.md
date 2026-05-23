+++
title = "Cloudsmith"
description = "Upload packages to Cloudsmith repositories"
weight = 82
template = "docs.html"
+++

Anodizer can upload deb, rpm, and apk packages to [Cloudsmith](https://cloudsmith.io/) repositories.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | structured warn line per (org, repo, filename) tuple (DELETE migration pending) | `CLOUDSMITH_API_KEY package_delete` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
cloudsmiths:
  - organization: myorg
    repository: myrepo
```

## Full config reference

```yaml
cloudsmiths:
  - organization: myorg          # required
    repository: releases         # required
    formats:                     # default: [apk, deb, rpm]
      - deb
      - rpm
    distributions:               # per-format distribution tag
      deb: "ubuntu/jammy"
      rpm: "el/8"
      alpine: "alpine/any-version"
    component: main              # deb only
    secret_name: CLOUDSMITH_TOKEN
    republish: true              # allow overwriting existing versions
    ids: []                      # filter by build IDs
    skip: false                  # skip this config
```

## Authentication

| Variable | Description |
|----------|-------------|
| `CLOUDSMITH_TOKEN` | Cloudsmith API key (or custom name via `secret_name`) |

## Common gotchas

- If `distributions` is omitted, packages are uploaded without a distribution tag; some Cloudsmith repo configurations require a valid distribution to index the package.
- The `component` field only affects deb packages. Setting it for rpm or apk has no effect.
- Format detection is by file extension: `.apk` maps to `alpine` (not `apk`) in the config.

## Republish / update behavior

Set `republish: true` when your release flow may re-cut a version — for example, after a CI failure mid-publish, a hotfix, or a rollback-and-retry. Without it, re-uploading a package with the same version and filename fails with an MD5 conflict.

```yaml
cloudsmiths:
  - organization: myorg
    repository: releases
    republish: true   # prevents MD5 conflict on version re-cut
```

By default (`republish: false`), Cloudsmith rejects any upload whose filename+version already exists in the repository.

## Cloudsmith config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `organization` | string | **required** | Cloudsmith organization name (template) |
| `repository` | string | **required** | Cloudsmith repository name (template) |
| `ids` | list | none | Filter by build IDs |
| `formats` | list | `["apk", "deb", "rpm"]` | Package format filter |
| `distributions` | map | none | Distribution mapping per format (e.g., `deb: "ubuntu/focal"`) |
| `component` | string | none | Debian component name (e.g., `"main"`) |
| `secret_name` | string | `CLOUDSMITH_TOKEN` | Environment variable name for the API key |
| `skip` | string/bool | none | Skip this config |
| `republish` | string/bool | `false` | Allow overwriting existing package versions |

## Format detection

Packages are matched by file extension:

| Extension | Format |
|-----------|--------|
| `.deb` | `deb` |
| `.rpm` | `rpm` |
| `.apk` | `alpine` |
| other | `raw` |

## Distribution mapping

Map package formats to specific distributions:

```yaml
cloudsmiths:
  - organization: myorg
    repository: myrepo
    distributions:
      deb: "ubuntu/focal"
      rpm: "el/8"
    component: main
```

## Full example

```yaml
cloudsmiths:
  - organization: myorg
    repository: releases
    formats:
      - deb
      - rpm
    distributions:
      deb: "ubuntu/jammy"
    component: main
    republish: true
```
