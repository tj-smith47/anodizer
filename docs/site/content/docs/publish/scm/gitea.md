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

`owner` and `name` are inferred from the `origin` git remote when omitted, so a block may name only a token override:

```yaml
release:
  gitea:
    token: "{{ .Env.RELEASE_GITEA_TOKEN }}"   # owner/name come from the origin remote
```

Gitea has no canonical public host, so the remote is read only when `gitea_urls.api` names its host (`https://gitea.mycorp.com/api/v1` reads a `gitea.mycorp.com` remote). Without `gitea_urls.api`, or with a remote on another host, nothing is filled and the block is refused naming the host.

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
- Existing release detection asks `GET /releases/tags/{tag}` first; on a 404 (no release, or a server older than 1.13 without that endpoint) it reads every listing page, 50 per page, and stops on a page that repeats
- `replace_existing_artifacts`: lists attachments, finds by name, deletes, then re-uploads
- Asset upload via multipart POST with the file as the `attachment` form field
- URL segments are percent-encoded for special characters
- 300-second HTTP timeout
- Release preflight: before the build, `GET /api/v1/repos/{owner}/{repo}` checks that the token can push (a response with no `permissions` passes). A failed check stops the run unless `release.preflight.fail_on_error` is `false`; see [Release preflight](@/docs/publish/github.md#release-preflight)

## Self-hosted Gitea

```yaml
gitea_urls:
  api: "https://gitea.mycompany.com"
```

`api` is the instance URL — anodizer appends `/api/v1/...` itself. A value
that already ends in `/api/v1` is accepted and trimmed back to the instance
root, so a deployment subpath (`https://example.com/forge/api/v1`) keeps its
subpath. A value with no scheme is refused.

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
