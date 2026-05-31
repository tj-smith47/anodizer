+++
title = "Makeself"
description = "Create self-extracting archives with makeself"
weight = 66
template = "docs.html"
+++

Anodizer can create self-extracting `.run` archives using [makeself](https://makeself.io/).

## Classification

Packager — creates self-extracting `.run` archives from Linux/macOS binaries. Required: not a publisher; `makeself` must be on PATH.

## Minimal config

```yaml
makeselfs:
  - script: ./install.sh
```

## Full config reference

```yaml
makeselfs:
  - id: default                      # optional; unique identifier
    ids: []                          # optional; filter by build IDs
    name_template: ""                # optional; output filename template
    name: ""                         # optional; display name embedded in archive
    script: ./scripts/install.sh     # required; startup script path (template)
    description: ""                  # optional; LSM metadata description
    maintainer: ""                   # optional; LSM metadata maintainer
    keywords: []                     # optional; LSM metadata keywords
    homepage: ""                     # optional; LSM metadata homepage URL
    license: ""                      # optional; LSM metadata license
    compression: ""                  # optional; gzip | bzip2 | xz | lzo | compress | none
    extra_args: []                   # optional; extra makeself CLI arguments
    files: []                        # optional; additional files to include
    os: ["linux", "darwin"]          # optional; target OS filter
    arch: []                         # optional; target architecture filter
    disable: false                   # optional
```

## Authentication

Not applicable — makeself archive creation is a local build step with no external service calls.

## Common gotchas

- **`makeself` must be on `PATH`**: the stage errors if `makeself` is not found.
- **One `.run` per platform**: artifacts are grouped by OS + arch; each group produces one self-extracting archive. Use `os` and `arch` to restrict which targets are packaged.
- **Artifact ordering**: the makeself stage uses a `BTreeMap` (sorted by platform key) internally to ensure the artifact registration order is deterministic across builds, preventing drift in `dist/artifacts.json`.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Makeself config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | `default` | Unique identifier |
| `ids` | list | all builds | Filter by build IDs |
| `name_template` | string | `{project}_{version}_{os}_{arch}.run` | Output filename (template) |
| `name` | string | project name | Display name embedded in the archive |
| `script` | string | **required** | Startup script path (template) |
| `description` | string | none | LSM metadata description |
| `maintainer` | string | none | LSM metadata maintainer |
| `keywords` | list | none | LSM metadata keywords |
| `homepage` | string | none | LSM metadata homepage URL |
| `license` | string | none | LSM metadata license |
| `compression` | string | makeself default | Compression: `gzip`, `bzip2`, `xz`, `lzo`, `compress`, or `none` |
| `extra_args` | list | none | Extra `makeself` CLI arguments |
| `files` | list | none | Additional files to include |
| `os` | list | `["linux", "darwin"]` | Target OS filter |
| `arch` | list | all | Target architecture filter |
| `disable` | string/bool | none | Disable this config |

### File entries

Each entry in `files` can specify:

| Field | Alias | Type | Description |
|-------|-------|------|-------------|
| `source` | `src` | string | Source file path |
| `destination` | `dst` | string | Destination path inside archive |
| `strip_parent` | — | bool | Strip parent directory from source path |

## Prerequisites

The `makeself` command must be installed and available on PATH.

## Behavior

- Groups binary artifacts by platform (os + arch), creating one `.run` per platform
- Generates an embedded LSM (Linux Software Map) metadata file
- The `.run` extension is auto-appended if not present in the output name
- IDs must be unique across all makeself configs
- Skippable with `--skip makeself`

## Compression

Override the default compression method:

```yaml
makeselfs:
  - script: ./install.sh
    compression: xz
```

## Full example

```yaml
makeselfs:
  - id: installer
    script: ./scripts/install.sh
    name: "My App Installer"
    description: "Self-extracting installer for My App"
    maintainer: "Alice <alice@example.com>"
    license: MIT
    homepage: "https://example.com/myapp"
    compression: xz
    os:
      - linux
    files:
      - src: config.example.yaml
        dst: config.yaml
      - src: LICENSE
    extra_args:
      - "--nox11"
```
