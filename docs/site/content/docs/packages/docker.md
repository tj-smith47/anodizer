+++
title = "Docker"
description = "Build and push multi-arch Docker images"
weight = 4
template = "docs.html"
+++

Anodize builds Docker images via `docker buildx`, supporting multi-architecture builds with tag templates.

## Minimal config

```yaml
crates:
  - name: myapp
    docker:
      - image_templates:
          - "myorg/myapp:{{ Version }}"
          - "myorg/myapp:latest"
        dockerfile: Dockerfile
```

## Docker config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image_templates` | list | — | Docker image tags (templates supported) |
| `dockerfile` | string | `Dockerfile` | Path to Dockerfile |
| `platforms` | list | none | Target platforms (e.g., `linux/amd64`, `linux/arm64`) |
| `binaries` | list | all | Which binaries to copy into the build context |
| `build_flag_templates` | list | none | Additional `docker buildx build` flags |
| `skip_push` | bool | `false` | Build but don't push |
| `extra_files` | list | none | Extra files to copy into build context |
| `push_flags` | list | none | Additional push flags |

## Multi-arch builds

```yaml
docker:
  - image_templates:
      - "myorg/myapp:{{ Version }}"
    dockerfile: Dockerfile
    platforms:
      - linux/amd64
      - linux/arm64
```

## Build flags

Pass additional flags to `docker buildx build`:

```yaml
docker:
  - image_templates:
      - "myorg/myapp:{{ Version }}"
    build_flag_templates:
      - "--build-arg=VERSION={{ Version }}"
      - "--label=org.opencontainers.image.version={{ Version }}"
```

## Full example

```yaml
crates:
  - name: myapp
    docker:
      - image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}"
          - "ghcr.io/myorg/myapp:latest"
        dockerfile: Dockerfile
        platforms:
          - linux/amd64
          - linux/arm64
        extra_files:
          - config.example.yaml
        build_flag_templates:
          - "--build-arg=VERSION={{ Version }}"
```
