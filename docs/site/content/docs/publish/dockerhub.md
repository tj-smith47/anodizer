+++
title = "Docker Hub"
description = "Sync descriptions to Docker Hub repositories"
weight = 83
template = "docs.html"
+++

Anodizer can sync short and full descriptions to your Docker Hub repositories. This does not build or push images; it updates the repository metadata that appears on the Docker Hub page.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | manual cleanup checklist (description PATCH cannot be programmatically reverted) | `DOCKER_TOKEN write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
dockerhub:
  - username: myuser
    images:
      - myorg/myapp
    description: "A fast CLI tool"
```

## Docker Hub config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `username` | string | **required** | Docker Hub username |
| `secret_name` | string | `DOCKER_PASSWORD` | Environment variable name for the password |
| `images` | list | none | Docker Hub repository names to update |
| `description` | string | `""` | Short description (max 100 characters) |
| `full_description` | object | none | Full description source (see below) |
| `disable` | string/bool | none | Disable this config |

### Full description sources

The `full_description` field supports loading content from a file or URL:

```yaml
# From a local file
full_description:
  from_file:
    path: README.md

# From a URL
full_description:
  from_url:
    url: "https://raw.githubusercontent.com/myorg/myapp/main/README.md"
    headers:
      Authorization: "token {{ .Env.GITHUB_TOKEN }}"
```

| Source | Fields |
|--------|--------|
| `from_file` | `path` — local file path |
| `from_url` | `url` — HTTP URL; `headers` — optional HTTP headers |

## Environment variables

| Variable | Description |
|----------|-------------|
| `DOCKER_PASSWORD` | Docker Hub password (or custom name via `secret_name`) |

## Behavior

- Authenticates via Docker Hub API (`hub.docker.com/v2/users/login/`)
- PATCHes each repository with the description fields
- Short descriptions longer than 100 characters trigger a warning (Docker Hub truncates)
- `from_file` takes precedence over `from_url` when both are set
- Skips the PATCH when both `description` and `full_description` are empty

## Full example

```yaml
dockerhub:
  - username: myuser
    images:
      - myorg/myapp
      - myorg/myapp-server
    description: "A fast CLI tool for data processing"
    full_description:
      from_file:
        path: README.md
```
