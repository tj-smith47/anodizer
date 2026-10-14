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
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ Version }}-arm64"
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
        skip_push: false             # optional; true | false | 1 | 0 | "auto" | template
        id: ""                       # optional; unique identifier
        use: docker                  # optional; docker | podman
        retry:
          attempts: 10               # optional; default 10
          delay: 10s                 # optional; base delay
          max_delay: 5m              # optional; delay cap
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
| `skip_push` | string/bool | none | Skip push: `true` or `1` skips, `auto` skips for prereleases, anything else pushes; a template is rendered and read the same way, in any letter case (see [bool-or-template values](@/docs/general/templates.md#bool-or-template-values)) |
| `id` | string | none | Unique identifier |
| `use` | string | `docker` | Backend: `"docker"` or `"podman"`. The `"podman"` backend is **Linux-only** — see [Podman backend](@/docs/packages/podman.md) for the full caveats and flag-compatibility table. |
| `retry` | object | see below | Retry config for manifest push |

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

## Push flags

`push_flags` are rendered as templates and placed after the manifest name on
the push command:

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
        push_flags: ["--purge", "--annotation=tag={{ .Tag }}"]
```

For tag `v1.2.3` this runs:

```text
docker manifest push ghcr.io/myorg/myapp:1.2.3 --purge --annotation=tag=v1.2.3
```

## Manifests with no images

An `image_templates` entry that renders empty is left out of the manifest.
When every entry renders empty, or the list itself is empty, the manifest is
skipped: no `manifest create` or `manifest push` runs, no artifact is
registered, the docker/podman binary is not needed, and the release
continues. For example, this manifest has no images outside a snapshot:

```yaml
crates:
  - name: myapp
    docker_manifests:
      - id: multi-arch
        name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "{{ if .IsSnapshot }}ghcr.io/myorg/myapp:snap{{ end }}"
```

Releasing version `1.2.3` (tag `myapp-v1.2.3`), the docker stage warns about
each blank entry:

```text
     Warning skipped image_template '{{ if .IsSnapshot }}ghcr.io/myorg/myapp:snap{{ end }}' for manifest 'ghcr.io/myorg/myapp:1.2.3' — rendered empty
```

and the skip is listed in the end-of-run summary, under the manifest's `id`
(or `index <N>` when it has none):

```text
   • 1 intentional skip:
   •   ↳ [docker-manifest] multi-arch — manifest has no images
```

Under `--show-skipped` the docker stage also prints the skip as it happens:

```text
     • skipped docker manifest 'multi-arch' — manifest has no images
```

## `skip_push`

Control when manifests are pushed:

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ Version }}-arm64"
        skip_push: auto  # skip push for pre-release versions
```

`skip_push` takes `true` or `false`, an unquoted `1` or `0` (loaded as
`true` and `false`), `auto`, or a template. A template is rendered and
trimmed, then read in any letter case: `true` or `1` skips the push, `auto`
skips it on a prerelease, and anything else, including `false`, `0` and an
empty render, pushes.

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
        skip_push: "{{ if IsSnapshot }}1{{ end }}"  # skip the push on a snapshot
      - name_template: "ghcr.io/myorg/myapp:latest"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
        skip_push: 0  # always push
```

A snapshot never pushes a manifest, whatever `skip_push` says. A manifest
whose push is skipped is still created locally, and `verify-release` does not
probe it on the registry.

## Full example

```yaml
crates:
  - name: myapp
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ Version }}-arm64"
        retry:
          attempts: 5
          delay: "5s"
          max_delay: "2m"
      - name_template: "ghcr.io/myorg/myapp:latest"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ Version }}-arm64"
```
