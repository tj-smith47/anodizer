+++
title = "Docker"
description = "Build and push multi-arch Docker images via dockers_v2"
weight = 4
template = "docs.html"
+++

Anodizer builds Docker images via `docker buildx`, producing multi-arch OCI
image indexes in a single push. The canonical (and only) surface is per-crate
`dockers_v2:`. The legacy GoReleaser V1 `dockers:` block is rejected at
config-load time with a migration error pointing here.

> The `docker_v2:` spelling is still accepted as a back-compat serde alias, so
> existing configs keep working.

## Classification

Packager — builds and pushes Docker images. Required: false (optional packager
stage).

## Placement

`dockers_v2:` is a **per-crate** field — it lives under `crates[].dockers_v2`,
not at the top level:

```yaml
crates:
  - name: myapp
    dockers_v2:
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

Workspace-wide defaults can be set under `defaults.dockers_v2:` (single struct,
deep-merged into each crate's first `dockers_v2[]` entry).

## Minimal config

`images` defaults to `ghcr.io/<owner>/<crate>` — the owner is resolved from
`release.github` or the `origin` git remote, and each crate gets its own name.
So a typical project only declares the `dockerfile` and `tags`:

```yaml
crates:
  - name: myapp
    dockers_v2:
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
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["docker.io/myorg/myapp"]   # override the ghcr.io default
        tags: ["{{ Version }}", "latest"]
```

## Full config reference

```yaml
crates:
  - name: myapp
    dockers_v2:
      - id: myapp                      # optional; unique handle (for --id filters)
        ids: [myapp]                    # optional; build-ID filter
        dockerfile: Dockerfile          # required; path to Dockerfile
        images:                         # optional; default ghcr.io/<owner>/<crate>
          - ghcr.io/myorg/myapp
        tags:                           # required; one image:tag per (image × tag)
          - "{{ Version }}"
          - "v{{ Version }}"
          - latest
        labels:                         # optional; --label key=value (merged over auto OCI labels)
          org.opencontainers.image.source: "https://github.com/myorg/myapp"
        oci_labels: true                # optional; auto-inject org.opencontainers.image.* (default true)
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
        skip: false                     # optional; bool or template (accepts deprecation-warned `disable:` alias)
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
  `dockers_v2:` (this page).
- **`skip: true`** (or `disable: true` via deprecation-warned back-compat
  alias) skips the entry entirely: nothing is staged, built or pushed, and
  the stage prints `skipped dockers_v2[<index>] for crate <name> — skip=true`.
  `skip` is a bool or a template; a render of `true` or `1`, trimmed and in
  any letter case, skips, and any other render builds (see
  [bool-or-template values](@/docs/general/templates.md#bool-or-template-values)).
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

## dockers_v2 config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | — | Unique handle for this entry (for `--id` filters) |
| `ids` | list | — | Build-ID filter: only include artifacts whose `id` is in this list |
| `dockerfile` | string | — | Path to Dockerfile (required) |
| `images` | list | `ghcr.io/<owner>/<crate>` | Base image names. Defaults to the per-crate ghcr.io image — owner from `release.github` or the `origin` remote. Empty (no tags emitted) when the owner can't be resolved. Set to override. |
| `tags` | list | — | Tag suffixes — one full image ref per (image × tag) |
| `labels` | map | none | OCI labels via `--label key=value`. Merged over the auto-injected `org.opencontainers.image.*` set; user keys win, and user labels are passed after the auto labels; see [Flag order](#flag-order) |
| `oci_labels` | bool/template | `true` | Auto-inject the predefined `org.opencontainers.image.*` labels. Set `false` to opt out. See [Auto-injected OCI labels](#auto-injected-oci-labels). |
| `annotations` | map | none | OCI annotations via `--annotation key=value`. A multi-platform build scopes a key to the image index unless it names its own buildx scope; see [Annotation scopes](#annotation-scopes) |
| `extra_files` | list | none | Extra files copied into the build context |
| `platforms` | list | host | Target platforms (`linux/amd64`, `linux/arm64`, ...). `linux/amd64` stages the `v1` build and `linux/amd64/v3` the build whose `amd64_variant` is `v3`, with tag suffix `amd64v3`; `linux/arm64/v8` is `linux/arm64`, since every aarch64 build is the v8 baseline, and is staged under `linux/arm64/`, which is what buildx puts in `$TARGETPLATFORM` for it. A platform with no matching build warns and stages nothing; an amd64 level outside `v1`..`v4` is a `check config` error. Two entries that stage under one directory (`linux/arm64` beside `linux/arm64/v8`, `linux/amd64` beside `linux/amd64/v1`, `linux/arm` beside `linux/arm/v7`) are a `check config` error too |
| `build_args` | map | none | `--build-arg KEY=VALUE` pairs, passed in the order of their template keys; see [Flag order](#flag-order). A key matching a global `ARG` also picks the base image `{{ BaseImage }}` names; see [Base image annotations](#base-image-annotations) |
| `retry` | object | top-level `retry:` | Per-pipe retry config (deprecated; prefer top-level) |
| `flags` | list | none | Arbitrary extra `docker buildx build` flags |
| `skip` | bool/template | `false` | Skip the entry: nothing is staged, built or pushed. `true` or `1` (an unquoted `1` too, or a template rendering either) skips. Accepts deprecation-warned `disable:` alias |
| `sbom` | bool/template | `false` | Add `--sbom=true` to buildx |
| `hooks` | object | none | `pre:` / `post:` hooks; see Hooks below |
| `use` | string | `buildx` | Backend: `buildx` or `podman` (Linux-only) |

## Auto-injected OCI labels

anodizer auto-injects the standard predefined `org.opencontainers.image.*`
labels on every `dockers_v2` build — you do not declare them by hand. Each
label is emitted only when its source value resolves:

| Label | Source |
|-------|--------|
| `org.opencontainers.image.created` | RFC 3339 UTC date: `SOURCE_DATE_EPOCH` when set, else the commit timestamp (in a `--snapshot` run, the HEAD commit timestamp, also on a dirty tree). It is never the wall clock, so the image stays byte-reproducible |
| `org.opencontainers.image.source` | release repo URL (SSH remotes normalized to their `https://` web base) |
| `org.opencontainers.image.revision` | released commit SHA |
| `org.opencontainers.image.version` | released version (matches the image tag) |
| `org.opencontainers.image.title` | the crate's name |
| `org.opencontainers.image.description` | `meta.description` (Cargo-derived) |
| `org.opencontainers.image.licenses` | `meta.license` |
| `org.opencontainers.image.url` | `meta.homepage` |
| `org.opencontainers.image.documentation` | `metadata.documentation` |
| `org.opencontainers.image.vendor` | the crate's first author, with any `<email>` suffix stripped |

The `documentation` label is driven by the top-level `metadata.documentation`
field:

```yaml
metadata:
  documentation: "https://tj-smith47.github.io/anodizer"
```

→ `--label org.opencontainers.image.documentation=https://tj-smith47.github.io/anodizer`.

**User labels always win.** Any key you set under `labels:` overrides the
auto-derived value for that key (the auto value never clobbers an explicit
user label). To suppress the whole auto-label set, set `oci_labels: false`:

```yaml
crates:
  - name: myapp
    dockers_v2:
      - dockerfile: Dockerfile
        tags: ["{{ Version }}"]
        oci_labels: false          # opt out of the predefined org.opencontainers.image.* labels
        labels:
          org.opencontainers.image.source: "https://github.com/myorg/myapp"  # set your own
```

`created` deriving from `SOURCE_DATE_EPOCH` (seeded by the build stage) is what
keeps two builds of the same commit byte-identical — a wall-clock `created`
would defeat reproducibility, so anodizer omits the label entirely when no
source date is resolvable rather than stamping the current time.

## Multi-arch builds

```yaml
crates:
  - name: myapp
    dockers_v2:
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
manifest lists from images that were not built by `dockers_v2` in the same run.

## Annotation scopes

buildx reads an annotation as `[types:]key=value`, where `types` is a
comma-separated list of `index`, `manifest`, `index-descriptor` and
`manifest-descriptor`, each optionally qualified with a platform. On a
multi-platform build, a key with no scope of its own is written to the image
index (`index:`), so `docker buildx imagetools inspect` shows it. A key that
names a scope is passed as written:

```yaml
crates:
  - name: myapp
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/myorg/myapp"]
        tags: ["{{ Version }}"]
        platforms: [linux/amd64, linux/arm64]
        annotations:
          org.opencontainers.image.description: "My app"            # index only
          "manifest:org.opencontainers.image.revision": "{{ FullCommit }}"
          "index,manifest:org.opencontainers.image.source": "https://github.com/myorg/myapp"
          "manifest[linux/amd64]:com.example.cpu": "x86-64"
```

With `FullCommit` at `4f1c2e9a7b3d5c8e0f6a1b2c3d4e5f6a7b8c9d0e`, the build
command carries:

```text
--annotation index,manifest:org.opencontainers.image.source=https://github.com/myorg/myapp
--annotation manifest:org.opencontainers.image.revision=4f1c2e9a7b3d5c8e0f6a1b2c3d4e5f6a7b8c9d0e
--annotation manifest[linux/amd64]:com.example.cpu=x86-64
--annotation index:org.opencontainers.image.description=My app
```

A prefix holding anything else (`bogus:foo`, or `index,bogus:foo`) is part of
the key, so it gets the `index:` scope too (`index:bogus:foo=…`). The key is
cut at its first `=` before its scope is read, as buildx cuts the flag, so a
`=` inside a platform qualifier ends the key there and `manifest[a=b]:foo`
names no scope. A single-platform build passes every key as written.

## Flag order

`build_args`, `labels` and `annotations` are maps, and anodizer passes each
one's pairs in the order of their keys as written in the config (byte order,
before rendering), whatever the keys render to. That keeps the build command
the same from run to run. A pair whose key or value renders empty or only
whitespace is left out.

Two keys that render the same name are both passed, in that order, so the
later one is the value docker keeps. `{{ BaseImage }}` reads the same last
value. With `oci_labels` on, the auto labels the config does not set come
first, sorted by key, and the `labels:` pairs follow in their own key order.

```yaml
crates:
  - name: myapp
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/myorg/myapp"]
        tags: ["{{ Version }}"]
        platforms: [linux/amd64]
        oci_labels: false
        build_args:
          VERSION: "{{ Version }}"
          BASE: debian:12
          "{{ if IsSnapshot }}BASE{{ end }}": busybox:1   # overrides BASE on a snapshot
          CACHE_BUST: ""                                   # renders empty, left out
        labels:
          org.opencontainers.image.vendor: "MyOrg"
          com.example.team: platform
```

A snapshot build of version `0.0.0-SNAPSHOT-293f00a` passes:

```text
--build-arg BASE=debian:12
--build-arg VERSION=0.0.0-SNAPSHOT-293f00a
--build-arg BASE=busybox:1
--label com.example.team=platform
--label org.opencontainers.image.vendor=MyOrg
```

The templated key sorts after `VERSION` as written, so its `BASE` comes last
and docker builds from `busybox:1`. Outside a snapshot it renders empty and
`BASE=debian:12` is the only `BASE`.

## Base image annotations

`{{ BaseImage }}` and `{{ BaseImageDigest }}` name the Dockerfile's final
`FROM` image. A global `ARG` (one declared before the first `FROM`) in that
image reference takes its value from `build_args` when the build arg renders
non-empty, as `docker build --build-arg` does, and from the `ARG` default
otherwise. Build args render before the base image resolves, so they cannot
reference `{{ BaseImage }}` themselves. They also render before `images` and
`tags`, so a build arg that fails to render stops the run even for a config
whose images all render empty, which would otherwise be skipped.

```dockerfile
ARG BASE=gcr.io/distroless/static-debian12
FROM ${BASE}
```

```yaml
crates:
  - name: myapp
    dockers_v2:
      - dockerfile: Dockerfile
        tags: ["{{ Version }}"]
        build_args:
          BASE: "{{ Env.BASE_IMAGE }}"
        annotations:
          org.opencontainers.image.base.name: "{{ BaseImage }}"
          org.opencontainers.image.base.digest: "{{ BaseImageDigest }}"
```

With `BASE_IMAGE=cgr.dev/chainguard/static:latest`, both annotations name
the Chainguard image (`BaseImageDigest` is its digest from
`docker buildx imagetools inspect`); with `BASE_IMAGE` empty, they name the
distroless default.

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
| `{{ Digest }}` | post only | The digest the registry stores for the build — the image manifest single-platform, the image index multi-platform. Read from buildx's `--metadata-file` (`containerimage.digest`), or from `podman push --digestfile` under `use: podman`. A build that exports no image at all (neither pushed nor loaded) has no digest to report, and a `post:` hook that needs it then fails the build |
| `{{ BaseImage }}` / `{{ BaseImageDigest }}` | pre + post | Final-stage base image |

## Dockerfile pattern (distroless + dist-tree binary)

The recommended pattern: a multi-stage-free Dockerfile that copies the
pre-built release binary from the context anodizer stages. The context holds
one binary per platform under the directory `$TARGETPLATFORM` spells
(`linux/amd64/myapp`, `linux/arm64/myapp`, `linux/arm/v7/myapp`,
`linux/amd64/v3/myapp`), so the `COPY` selects by `TARGETOS`, `TARGETARCH`
and, when buildx sets one, `TARGETVARIANT`. The `BIN` build-arg names the
binary.

```dockerfile
# syntax=docker/dockerfile:1.7
FROM --platform=$TARGETPLATFORM gcr.io/distroless/cc-debian12:nonroot

ARG TARGETOS
ARG TARGETARCH
ARG TARGETVARIANT
ARG BIN=myapp
COPY ${TARGETOS}/${TARGETARCH}${TARGETVARIANT:+/$TARGETVARIANT}/${BIN} /usr/local/bin/myapp

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
crates:
  - name: myapp
    dockers_v2:
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
    dockers_v2:
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
