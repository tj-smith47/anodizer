+++
title = "Docker Digests"
description = "Configure Docker image digest artifact creation"
weight = 69
template = "docs.html"
+++

After pushing Docker images, Anodizer captures the image digest (sha256 hash) and writes it to artifact files. Digests provide immutable references to images, useful for signing and pinning.

## Classification

Packager — post-push artifact that captures the sha256 digest of pushed Docker images. Required: enabled by default alongside the docker stage.

## Minimal config

Digest creation is enabled by default. To customize:

```yaml
crates:
  - name: myapp
    docker_digest:
      name_template: "{{ .ProjectName }}_{{ .Version }}_digest.txt"
```

## Docker digest config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `disable` | string/bool | `false` | Disable digest artifact creation |
| `name_template` | string | tag-based naming | Template for digest artifact filename |

## Behavior

- After each Docker image push, the sha256 digest is extracted via `docker inspect`
- A per-tag digest file is written to the dist directory
- A combined `digests.txt` file is written containing all digest lines
- Digest files are registered as `docker_digest` artifacts
- The digest is also stored in artifact metadata under the `digest` key

## Full config reference

```yaml
crates:
  - name: myapp
    docker_digest:
      name_template: ""              # optional; template for digest artifact filename
      disable: false                 # optional; bool or template string
```

## Disabling digests

```yaml
crates:
  - name: myapp
    docker_digest:
      skip: true
```

## Authentication

Not applicable — digest capture reads from the local Docker daemon (`docker inspect`). No registry credentials are required at this stage; authentication happens during the prior image push.

## Common gotchas

- The digest is captured immediately after push via `docker inspect`. If the push did not complete, the digest may be missing or stale.
- Digest files are written to `dist/` and registered as `docker_digest` artifacts; they can be referenced in subsequent signing stages via artifact IDs.

## Republish / update behavior

Not applicable — this is a local artifact-capture step, not a publisher.

## Using digests

Digests are primarily useful for:

- **Docker signing** — tools like `cosign` sign by digest rather than tag
- **Manifest pinning** — Docker manifests use digests to reference specific image layers
- **Immutable references** — digests guarantee the exact image content, unlike mutable tags
