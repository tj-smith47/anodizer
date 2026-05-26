+++
title = "Docker Manifests"
description = "Create multi-architecture Docker manifest lists"
weight = 70
template = "docs.html"
+++

Docker manifests combine platform-specific images into a single multi-architecture reference, so users can `docker pull` and get the right image for their platform automatically.

## Classification

Packager + publisher — creates and pushes Docker manifest lists combining multi-arch images. Required: false (optional stage).

## Minimal config

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ .Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ .Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ .Version }}-arm64"
```

## Full config reference

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "myorg/myapp:{{ Version }}"  # required; manifest tag (template)
        image_templates:             # required; image references to include (templates)
          - "myorg/myapp:{{ Version }}-amd64"
          - "myorg/myapp:{{ Version }}-arm64"
        create_flags: []             # optional; extra flags for docker manifest create
        push_flags: []               # optional; extra flags for docker manifest push
        skip_push: false             # optional; true | false | "auto"
        id: ""                       # optional; unique identifier
        use: docker                  # optional; docker | podman
        retry:
          attempts: 10               # optional; default 10
          delay: 10s                 # optional; base delay
          max_delay: 5m              # optional; delay cap
        disable: false               # optional
```

## Authentication

Docker registry credentials are resolved from the host Docker configuration (`~/.docker/config.json`). Run `docker login` before releasing.

## Common gotchas

- **Image references must exist**: all `image_templates` must already be pushed before the manifest stage runs. Anodizer cross-checks the list against pushed images and emits "did you mean?" suggestions for near-misses.
- **Digest pinning**: anodizer pins image references to their sha256 digest when available. If a digest is missing (e.g., the docker-digests stage was skipped), a tag reference is used with a warning.
- **Retry**: manifest push may fail transiently on busy registries. The built-in retry (default 10 attempts, 10s base delay) handles most transient errors.

## Republish / update behavior

Not applicable as a config flag — the stage removes any existing manifest before recreating it. Re-running is idempotent (the old manifest is deleted first, preventing stale manifest errors).

## Docker manifest config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | — | Manifest name/tag (template) |
| `image_templates` | list | — | Image references to include (templates) |
| `create_flags` | list | none | Extra flags for `docker manifest create` (templates) |
| `push_flags` | list | none | Extra flags for `docker manifest push` (templates) |
| `skip_push` | string/bool | none | Skip push: `true`, `false`, or `"auto"` (skip for prereleases) |
| `id` | string | none | Unique identifier |
| `use` | string | `docker` | Backend: `"docker"` or `"podman"`. The `"podman"` backend is **Linux-only** — see [Podman backend](@/docs/packages/podman.md) for the full caveats and flag-compatibility table. |
| `retry` | object | see below | Retry config for manifest push |
| `disable` | string/bool | none | Disable this manifest |

### Retry config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `attempts` | integer | `10` | Maximum retry attempts |
| `delay` | string | `10s` | Base delay between retries |
| `max_delay` | string | `5m` | Maximum delay cap |

## Behavior

- Runs during the publishing phase, after images are pushed
- Removes any existing manifest first (prevents stale manifest errors on re-runs)
- Pins image references to their sha256 digest when available, falling back to tag references with a warning
- Provides "did you mean?" suggestions when image references don't match any pushed images
- Uses exponential backoff retry for transient registry errors
- Both `create_flags` and `push_flags` are template-rendered

## Skip push

Control when manifests are pushed:

```yaml
docker_manifests:
  - name_template: "ghcr.io/myorg/myapp:{{ .Version }}"
    image_templates:
      - "ghcr.io/myorg/myapp:{{ .Version }}-amd64"
      - "ghcr.io/myorg/myapp:{{ .Version }}-arm64"
    skip_push: auto  # skip push for pre-release versions
```

## Full example

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ .Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ .Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ .Version }}-arm64"
        retry:
          attempts: 5
          delay: "5s"
          max_delay: "2m"
      - name_template: "ghcr.io/myorg/myapp:latest"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ .Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ .Version }}-arm64"
```
