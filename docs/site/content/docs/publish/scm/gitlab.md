+++
title = "GitLab"
description = "Create releases and upload assets to GitLab"
weight = 1
template = "docs.html"
+++

Anodizer can create releases and upload assets to GitLab repositories.

## Minimal config

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
```

## GitLab-specific config

The `release` config is shared across all SCM providers. See the [GitHub releases](/docs/publish/github/) page for the full list of shared fields. The GitLab-specific fields are:

### GitLab URLs (`gitlab_urls`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api` | string | `https://gitlab.com/api/v4/` | GitLab API base URL |
| `download` | string | `https://gitlab.com` | GitLab download base URL |
| `skip_tls_verify` | bool | `false` | Skip TLS certificate verification |
| `use_package_registry` | bool | `false` | Upload to the Package Registry instead of project uploads |
| `use_job_token` | bool | `false` | Authenticate with `CI_JOB_TOKEN` instead of personal token |

## Environment variables

| Variable | Description |
|----------|-------------|
| `GITLAB_TOKEN` | Personal access token or project token |
| `CI_JOB_TOKEN` | CI job token (when `use_job_token: true`) |

## Upload methods

GitLab supports two upload methods:

- **Project Uploads** (default) — uploads via `POST /projects/{id}/uploads` (multipart), then creates release links
- **Package Registry** — uploads via `PUT /projects/{id}/packages/generic/{package}/{version}/{filename}`

```yaml
gitlab_urls:
  use_package_registry: true
```

## Release modes

Control how release notes are handled when a release already exists:

| Mode | Behavior |
|------|----------|
| `keep-existing` | Keep existing release body unchanged |
| `append` | Append new content after existing body |
| `prepend` | Prepend new content before existing body |
| `replace` | Replace existing body entirely |

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
  mode: append
```

## Behavior

- GitLab does not support draft releases
- Existing release detection: checks by tag; creates new on 403/404, updates on 200
- `replace_existing_artifacts`: on conflict (400/422), deletes the conflicting link and retries
- URL-encoded project paths (e.g., `group/subgroup/project` becomes `group%2Fsubgroup%2Fproject`)
- 300-second HTTP timeout

## Self-hosted GitLab

```yaml
gitlab_urls:
  api: "https://gitlab.mycompany.com/api/v4/"
  download: "https://gitlab.mycompany.com"
```

## Full example

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
  name_template: "{{ ProjectName }} {{ Version }}"
  header:
    from_file: RELEASE_HEADER.md
  mode: replace
  replace_existing_artifacts: true

gitlab_urls:
  use_package_registry: true
  use_job_token: true
```
