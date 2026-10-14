+++
title = "Retry"
description = "Automatic retry with exponential backoff for uploads and Docker operations"
weight = 9
template = "docs.html"
+++

Anodizer automatically retries failed operations with exponential backoff. Retry behavior is built into upload and Docker stages.

## Rate limits

A `429 Too Many Requests` waits the time the server asked for in place of the
backoff step, on every HTTP retry: release uploads, publishers (PyPI,
GemFury and Chocolatey included), preflight probes and announcers. The wait
is read from:

| Header | Read as |
|---|---|
| `RateLimit-Reset` | a unix timestamp; used when it is in the future |
| `Retry-After` | delta-seconds (an HTTP-date value is not read) |

`RateLimit-Reset` wins when both are present. Either header is read on a
429 only. A wait the run's deadline cannot hold ends the retries at once.

## Retry output

Each failed attempt prints a warning of the form
`<call> attempt <n>/<max> failed (<cause>); retrying in <delay>`, and a
ladder that runs out prints `<call> failed after <n> attempts, giving up`.
A call that succeeds after a retry prints `<call> succeeded after <n>
attempts`. The error a spent ladder returns names the call once and ends in
`exhausted retry attempts`. A call that fails fast on its first attempt (a
4xx other than 429) returns its own error without that phrase, since no
retry ran. Each HTTP failure's message is printed once in the error chain.

## Release uploads

Release asset uploads and the release API calls on GitHub, GitLab and Gitea
read the top-level `retry:` block. Its defaults:

| Field | Default |
|-------|---------|
| `attempts` | 10 (the first attempt included) |
| `delay` | 10s before the second attempt, doubling after each failure |
| `max_delay` | 5m cap on any one wait |
| `max_elapsed` | 15m cap on the total time spent retrying one operation |

```yaml
retry:
  attempts: 5
  delay: 2s
  max_delay: 1m
  max_elapsed: 10m
```

## Docker retry

Docker build and push operations support configurable retry via the `retry` field:

```yaml
crates:
  - name: myapp
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/owner/myapp"]
        tags: ["{{ Version }}"]
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
- `dockers_v2[]` — Docker Buildx builds (canonical)
- `docker_manifests[]` — Docker manifest creation and push (legacy stitching only)

### Duration format

Delay values accept duration strings with these suffixes:

| Suffix | Unit | Example |
|--------|------|---------|
| `ms` | milliseconds | `500ms` |
| `s` | seconds | `10s` |
| `m` | minutes | `2m` |

A bare number without a suffix is treated as seconds (e.g., `10` = `10s`).
