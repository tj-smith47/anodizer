+++
title = "Gitea"
description = "Create releases and upload assets to Gitea"
weight = 2
template = "docs.html"
+++

Anodizer can create releases and upload assets to Gitea repositories.

## Minimal config

```yaml
release:
  gitea:
    owner: myorg
    name: myrepo
```

## Gitea-specific config

The `release` config is shared across all SCM providers. See the [GitHub releases](/docs/publish/github/) page for the full list of shared fields. The Gitea-specific fields are:

### Gitea URLs (`gitea_urls`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api` | string | auto-derived | Gitea API base URL |
| `download` | string | derived from api | Gitea download base URL |
| `skip_tls_verify` | bool | `false` | Skip TLS certificate verification |

## Environment variables

| Variable | Description |
|----------|-------------|
| `GITEA_TOKEN` | Gitea API token |

## Release modes

Control how release notes are handled when a release already exists:

| Mode | Behavior |
|------|----------|
| `keep-existing` | Keep existing release body unchanged |
| `append` | Append new content after existing body |
| `prepend` | Prepend new content before existing body |
| `replace` | Replace existing body entirely |

## Behavior

- Supports draft and prerelease flags (unlike GitLab)
- Existing release detection uses paginated listing (up to 10 pages, 50 per page)
- `replace_existing_artifacts`: lists attachments, finds by name, deletes, then re-uploads
- Asset upload via multipart POST with the file as the `attachment` form field
- URL segments are percent-encoded for special characters
- 300-second HTTP timeout

## Self-hosted Gitea

```yaml
gitea_urls:
  api: "https://gitea.mycompany.com"
```

## Full example

```yaml
release:
  gitea:
    owner: myorg
    name: myrepo
  name_template: "{{ ProjectName }} {{ Version }}"
  draft: true
  prerelease: auto
  header:
    from_file: RELEASE_HEADER.md
  mode: replace
  replace_existing_artifacts: true

gitea_urls:
  api: "https://gitea.mycompany.com"
```
