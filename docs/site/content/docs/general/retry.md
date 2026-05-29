+++
title = "Retry"
description = "Automatic retry with exponential backoff for uploads and Docker operations"
weight = 9
template = "docs.html"
+++

Anodizer automatically retries failed operations with exponential backoff. Retry behavior is built into upload and Docker stages.

## Release uploads

All release asset uploads (GitHub, GitLab, Gitea) use automatic retry with these defaults:

| Parameter | Value |
|-----------|-------|
| Max attempts | 10 |
| Initial delay | 50ms |
| Max delay | 30s |
| Backoff | Exponential (delay × 2^(attempt-1)) |

All upload errors are retried — not just transient HTTP errors. This matches GoReleaser's behavior of wrapping all upload failures as retriable.

There is no user-facing configuration for release upload retries; the defaults are always applied.

## Docker retry

Docker build and push operations support configurable retry via the `retry` field:

```yaml
crates:
  - name: myapp
    docker_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/owner/myapp"]
        tags: ["{{ .Version }}"]
        retry:
          attempts: 10
          delay: "10s"
          max_delay: "5m"
```

### Docker retry config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `attempts` | integer | `10` | Maximum number of retry attempts |
| `delay` | string | `10s` | Base delay between retries (e.g., `500ms`, `1s`, `2m`) |
| `max_delay` | string | `5m` | Maximum delay cap for exponential backoff |

The same retry config is available on:
- `docker_v2[]` — Docker Buildx builds (canonical)
- `docker_manifests[]` — Docker manifest creation and push (legacy stitching only)

### Duration format

Delay values accept duration strings with these suffixes:

| Suffix | Unit | Example |
|--------|------|---------|
| `ms` | milliseconds | `500ms` |
| `s` | seconds | `10s` |
| `m` | minutes | `2m` |

A bare number without a suffix is treated as seconds (e.g., `10` = `10s`).
