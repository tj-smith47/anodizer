+++
title = "Docker"
description = "Build and push multi-arch Docker images via docker_v2"
weight = 4
template = "docs.html"
+++

Anodizer builds Docker images via `docker buildx`, producing multi-arch OCI
image indexes in a single push. The canonical (and only) surface is per-crate
`docker_v2:`. The legacy GoReleaser V1 `dockers:` block is rejected at
config-load time with a migration error pointing here.

## Classification

Packager — builds and pushes Docker images. Required: false (optional packager
stage).

## Placement

`docker_v2:` is a **per-crate** field — it lives under `crates[].docker_v2`,
not at the top level:

```yaml
crates:
  - name: myapp
    docker_v2:
      - id: myapp
        dockerfile: Dockerfile
        images:
          - ghcr.io/myorg/myapp
        tags:
          - "{{ Version }}"
          - latest
        platforms:
          - linux/amd64
          - linux/arm64
```

Workspace-wide defaults can be set under `defaults.docker_v2:` (single struct,
deep-merged into each crate's first `docker_v2[]` entry).

## Minimal config

`images` defaults to `ghcr.io/<owner>/<crate>` — the owner is resolved from
`release.github` or the `origin` git remote, and each crate gets its own name.
So a typical project only declares the `dockerfile` and `tags`:

```yaml
crates:
  - name: myapp
    docker_v2:
      - dockerfile: Dockerfile
        tags: ["{{ Version }}", "latest"]
```

Set `images` explicitly to publish under a different registry/name (e.g.
`docker.io/<owner>/<name>`), or when the owner can't be resolved (no GitHub
remote and no `release.github`) — an unresolvable owner leaves `images` empty
and the pipe emits no tags.

```yaml
crates:
  - name: myapp
    docker_v2:
      - dockerfile: Dockerfile
        images: ["docker.io/myorg/myapp"]   # override the ghcr.io default
        tags: ["{{ Version }}", "latest"]
```

## Full config reference

```yaml
crates:
  - name: myapp
    docker_v2:
      - id: myapp                      # optional; unique handle (for --id filters)
        ids: [myapp]                    # optional; build-ID filter
        dockerfile: Dockerfile          # required; path to Dockerfile
        images:                         # optional; default ghcr.io/<owner>/<crate>
          - ghcr.io/myorg/myapp
        tags:                           # required; one image:tag per (image × tag)
          - "{{ Version }}"
          - "v{{ Version }}"
          - latest
        labels:                         # optional; --label key=value
          org.opencontainers.image.source: "https://github.com/myorg/myapp"
        annotations:                    # optional; --annotation key=value
          org.opencontainers.image.licenses: "MIT"
        extra_files:                    # optional; copied into build context
          - LICENSE
          - README.md
        platforms:                      # optional; --platform list
          - linux/amd64
          - linux/arm64
        build_args:                     # optional; --build-arg KEY=VALUE
          VERSION: "{{ Version }}"
          BIN: anodizer
        retry:                          # optional; per-pipe retry
          attempts: 10
          delay: "10s"
          max_delay: "5m"
        flags:                          # optional; raw extra buildx flags
          - --provenance=false
        skip: false                     # optional; bool or template (accepts `disable:` alias)
        sbom: true                      # optional; --sbom=true
        hooks:                          # optional; pre/post hooks
          pre:
            - cmd: ./scripts/prepare-build-context.sh
              dir: .
          post:
            - cmd: echo "built {{ Images | join(sep=',') }}"
        use: buildx                     # optional; "buildx" (default) | "podman" (Linux-only)
```

## Authentication

Docker registry credentials are resolved from the host Docker configuration
(`~/.docker/config.json`). Run `docker login` (or use
[`docker/login-action@v3`](https://github.com/docker/login-action) in CI)
before the release step. In `anodizer-action`, set `docker-registry` /
`docker-username` / `docker-password` and the action logs in for you.

## Common gotchas

- **`docker buildx` required**: multi-architecture builds require Docker
  Buildx. Ensure the buildx plugin is installed and a builder with multi-arch
  support is configured. In GHA, use `docker/setup-qemu-action@v3` +
  `docker/setup-buildx-action@v3`.
- **No legacy `dockers:`**: the top-level GoReleaser V1 `dockers:` block is
  rejected at config-load time with a clear migration error. Port to
  `docker_v2:` (this page).
- **`skip: true`** (or `disable: true` via back-compat alias) builds the
  image locally but does not push.
- **Platform strings**: use Docker platform notation (`linux/amd64`,
  `linux/arm64`), not Rust target triples.
- **Build-args leak**: buildx records `build_args` in image history by
  default. Prefer `{{ Env.VAR }}` over raw user-config strings for secrets.
- **`use: podman` is Linux-only**: configs setting `use: podman` on macOS or
  Windows fail at config-validation time. The podman backend disables
  buildx-only flags (`--rewrite-timestamp`, `--provenance`, `--attest`,
  `--cache-from/to`, `--output`, `--sbom`).

## Republish / update behavior

Pushing the same `image:tag` to a registry overwrites the previous push.
Re-running the docker stage with the same `images` / `tags` re-pushes the
image. There is no `replace_existing_*` flag — registry semantics handle it.

## docker_v2 config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | — | Unique handle for this entry (for `--id` filters) |
| `ids` | list | — | Build-ID filter: only include artifacts whose `id` is in this list |
| `dockerfile` | string | — | Path to Dockerfile (required) |
| `images` | list | `ghcr.io/<owner>/<crate>` | Base image names. Defaults to the per-crate ghcr.io image — owner from `release.github` or the `origin` remote. Empty (no tags emitted) when the owner can't be resolved. Set to override. |
| `tags` | list | — | Tag suffixes — one full image ref per (image × tag) |
| `labels` | map | none | OCI labels via `--label key=value` |
| `annotations` | map | none | OCI annotations via `--annotation key=value` |
| `extra_files` | list | none | Extra files copied into the build context |
| `platforms` | list | host | Target platforms (`linux/amd64`, `linux/arm64`, ...) |
| `build_args` | map | none | `--build-arg KEY=VALUE` pairs |
| `retry` | object | top-level `retry:` | Per-pipe retry config (deprecated; prefer top-level) |
| `flags` | list | none | Arbitrary extra `docker buildx build` flags |
| `skip` | bool/template | `false` | Skip the build. Accepts `disable:` alias |
| `sbom` | bool/template | `false` | Add `--sbom=true` to buildx |
| `hooks` | object | none | `pre:` / `post:` hooks; see Hooks below |
| `use` | string | `buildx` | Backend: `buildx` or `podman` (Linux-only) |

## Multi-arch builds

```yaml
crates:
  - name: myapp
    docker_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/myorg/myapp"]
        tags: ["{{ Version }}"]
        platforms:
          - linux/amd64
          - linux/arm64
```

A single `docker buildx build --platform=linux/amd64,linux/arm64 --push ...`
emits one multi-arch OCI image index — no separate
[`docker_manifests[]`](./docker-manifests.md) entry is required.
`docker_manifests[]` is retained only for the niche case of stitching together
manifest lists from images that were not built by `docker_v2` in the same run.

## Hooks

`pre:` runs after the staging context is prepared but before
`docker buildx build`; `post:` runs after the image digest is captured. Hook
commands, working directories, and env values are template-expanded; in
addition to the standard template surface, hooks see:

| Variable | Available in | Description |
|----------|--------------|-------------|
| `{{ Images }}` | pre + post | List of `image:tag` references for this build |
| `{{ Dockerfile }}` | pre + post | Path to the rendered Dockerfile |
| `{{ ContextDir }}` | pre + post | Path to the buildx context staging directory |
| `{{ Digest }}` | post only | Image manifest digest |
| `{{ BaseImage }}` / `{{ BaseImageDigest }}` | post only | Final-stage base image (mirrors GoReleaser's overlay) |

## Dockerfile pattern (distroless + dist-tree binary)

The recommended pattern: a multi-stage-free Dockerfile that copies the
pre-built release binary from anodize's dist tree. The `BIN` build-arg names
the per-arch binary path.

```dockerfile
# syntax=docker/dockerfile:1.7
FROM --platform=$TARGETPLATFORM gcr.io/distroless/cc-debian12:nonroot

ARG BIN=myapp
COPY ${BIN} /usr/local/bin/myapp

USER nonroot
ENTRYPOINT ["/usr/local/bin/myapp"]
```

When this image doubles as an MCP server (the dogfood case), set
`ENTRYPOINT` to the binary and `CMD` to the MCP subcommand:

```dockerfile
ENTRYPOINT ["/usr/local/bin/anodizer"]
CMD ["mcp"]
```

Consumers then run `docker run --rm -i ghcr.io/myorg/myapp:<ver>` and the
container speaks MCP over stdio out of the box. See
[MCP registry](../publish/mcp-registry.md) for the manifest that points at
this image.

## Build flags

Pass additional flags to `docker buildx build`:

```yaml
docker_v2:
  - dockerfile: Dockerfile
    images: ["ghcr.io/myorg/myapp"]
    tags: ["{{ Version }}"]
    flags:
      - --provenance=false
      - "--label=org.opencontainers.image.version={{ Version }}"
```

Prefer the `labels:` / `annotations:` / `build_args:` maps over inline
`--label=` / `--build-arg=` in `flags:` — the maps are template-expanded per
entry and cleaner to read.

## Full example

```yaml
crates:
  - name: myapp
    docker_v2:
      - id: myapp
        dockerfile: Dockerfile
        images:
          - ghcr.io/myorg/myapp
        tags:
          - "{{ Version }}"
          - "v{{ Version }}"
          - latest
        platforms:
          - linux/amd64
          - linux/arm64
        build_args:
          BIN: myapp
          VERSION: "{{ Version }}"
        labels:
          org.opencontainers.image.source: "https://github.com/myorg/myapp"
          org.opencontainers.image.version: "{{ Version }}"
        annotations:
          org.opencontainers.image.licenses: "MIT"
        extra_files:
          - LICENSE
          - README.md
        sbom: true
```
