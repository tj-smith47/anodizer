+++
title = "Retry"
description = "Retry with exponential backoff for release calls, publishers, probes and Docker operations"
weight = 9
template = "docs.html"
+++

Anodizer retries a failed operation with exponential backoff: release API
calls and asset uploads, publishers, preflight probes, announcers and Docker
builds and pushes.

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
read the top-level `retry:` block, and so does every publisher. Its defaults:

| Field | Default |
|-------|---------|
| `attempts` | 10 (the first attempt included) |
| `delay` | 10s before the second attempt, doubling after each failure |
| `max_delay` | 5m cap on any one wait |
| `max_elapsed` | 15m deadline shared by every retry in one publisher run, or in one crate's release |

```yaml
retry:
  attempts: 5
  delay: 2s
  max_delay: 1m
  max_elapsed: 10m
```

## Retry budget

`max_elapsed` is a deadline for a whole run of something, shared by every
retry inside it:

| Run | Deadline |
|---|---|
| A publisher's `run`, `reconcile`, `rollback` or `preflight` | the moment it starts, plus `max_elapsed` |
| One crate's GitHub, GitLab or Gitea release (the release stage) | the moment that crate's release starts, plus `max_elapsed` |
| The release stage run by the `github-release` publisher (`--publish-only`) | the publisher's deadline, shared by every crate |
| Any other retry | the moment that retry starts, plus `max_elapsed` |

Every retried request inside the run shares the deadline: the auth exchange,
the publish request and the cleanup of one publisher, or the API calls and
asset uploads of one release. The next publisher, and the next crate's
release, start a fresh deadline. Retrying stops before a wait would pass the
deadline, and the run fails with the last error, so an idempotent re-run can
resume. A release whose CI job has a hard timeout should keep `max_elapsed`
below it.

## Duration format

Every duration field in the config (`retry.delay`, `retry.max_delay`,
`retry.max_elapsed`, `announce.deadline`, `release.upload_pace`, the notarize
`timeout`, `publish.cargo.wait_for_workspace_deps.max_wait` and
`poll_interval`, and the chocolatey and winget `post_publish_poll.interval`
and `timeout`) and the `--timeout` flag of
`anodizer release` and `anodizer build` read one format: one or more whole
numbers, each followed by a unit.

| Unit | Meaning | Example |
|------|---------|---------|
| `ms` | milliseconds | `500ms` |
| `s` | seconds | `90s` |
| `m` | minutes | `10m` |
| `h` | hours | `3h` |
| `d` | days | `2d` |

Terms add up, so `1h30m` is 90 minutes. Spaces and tabs may separate the
parts (`1h 30m`); no other whitespace is accepted. A number needs a unit, so
`90` is refused, and a fraction such as `1.5h` is refused. The longest
duration accepted is one year, `365d` (31536000s); a longer value is refused
when the config loads, or when `--timeout` is read, with a message naming the
value and the limit.

```bash
anodizer release --timeout 2d
```

## Docker retry

Docker build and push operations also read a per-image `retry` field. It is
deprecated in favour of the top-level `retry:` block and prints a warning once
per run. When it is set it replaces the top-level block for that image:

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

### Docker duration format

The two Docker `retry` delays read the [duration format](#duration-format)
above, with the same one-year limit. A bare number without a unit is also
accepted there and read as seconds (`10` is `10s`).
