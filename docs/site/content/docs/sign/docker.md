+++
title = "Docker Images"
description = "Sign Docker images with cosign"
weight = 2
template = "docs.html"
+++

Sign your Docker images after they're pushed.

## Config

```yaml
docker_signs:
  - artifacts: all
    cmd: cosign
    args: ["sign", "--key=cosign.key", "${artifact}"]
```

## Docker sign config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `artifacts` | string | `none` | What to sign: `none`, `all` |
| `cmd` | string | — | Signing command |
| `args` | list | — | Arguments (templates supported) |
