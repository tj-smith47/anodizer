+++
title = "Docker"
description = "Build and push multi-arch Docker images"
weight = 4
template = "docs.html"
+++

Anodizer builds Docker images via `docker buildx`, supporting multi-architecture builds with tag templates.

## Classification

Packager — builds and pushes Docker images. Required: false (optional packager stage).

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

## Authentication

Docker registry credentials are resolved from the host Docker configuration (`~/.docker/config.json`). Run `docker login` before releasing or set `DOCKER_USERNAME` / `DOCKER_PASSWORD` and call `docker login` in a `before:` hook.

## Common gotchas

- **`docker buildx` required**: multi-architecture builds require Docker Buildx. Ensure the buildx plugin is installed and a builder with multi-arch support is configured.
- **`skip_push: true`**: builds the image locally but does not push. Useful for testing the build without publishing.
- **Platform strings**: use Docker platform notation (`linux/amd64`, `linux/arm64`), not Rust target triples.

## Republish / update behavior

Not applicable as a config flag — pushing the same tag to a registry overwrites the previous image. Re-running the docker stage with the same `image_templates` re-pushes the image.

## Full config reference

```yaml
crates:
  - name: myapp
    docker:
      - image_templates:              # required; Docker image tags (templates supported)
          - "myorg/myapp:{{ Version }}"
        dockerfile: Dockerfile        # optional; path to Dockerfile
        platforms: []                 # optional; e.g. linux/amd64, linux/arm64
        binaries: []                  # optional; binaries to copy (default: all)
        build_flag_templates: []      # optional; additional docker buildx build flags
        skip_push: false              # optional; build but don't push
        extra_files: []               # optional; extra files to copy into build context
        push_flags: []                # optional; additional push flags
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
