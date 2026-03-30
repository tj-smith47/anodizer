+++
title = "Snapcraft"
description = "Build and publish Snap packages for the Snapcraft store"
weight = 60
template = "docs.html"
+++

The snapcraft stage builds `.snap` packages from your Linux binaries and optionally publishes them to the [Snap Store](https://snapcraft.io/).

## Required tools

- `snapcraft` — must be installed and on `PATH`. Install via `sudo snap install snapcraft --classic`.

## Platform

Snapcraft only runs against Linux binary artifacts. Builds targeting other operating systems are ignored.

## Minimal config

```yaml
crates:
  - name: myapp
    snapcrafts:
      - summary: "My application"
        description: "A longer description shown in the Snap Store."
```

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `name` | string | binary name | Snap package name in the store. |
| `title` | string | | User-facing application title. |
| `summary` | string | `"<name> snap package"` | Single-line description (max 79 characters). |
| `description` | string | | Extended description shown in the store. |
| `icon` | string | | Path to the snap icon image file. |
| `base` | string | `core22` | Base snap: `core`, `core18`, `core20`, `core22`, `core24`, `bare`. |
| `grade` | string | | Release quality: `stable` or `devel`. |
| `license` | string | | SPDX license identifier. |
| `confinement` | string | `strict` | Security model: `strict`, `devmode`, or `classic`. |
| `plugs` | list | | Interface permissions (e.g. `home`, `network`, `personal-files`). |
| `slots` | list | | Shared interface slots for other snaps. |
| `assumes` | list | | Required snapd features or minimum versions. |
| `apps` | map | auto | Named app entries. Auto-generates a default entry from the first binary if omitted. |
| `layouts` | map | | Filesystem layout mappings for sandbox accessibility. |
| `extra_files` | list | | Additional static files to bundle in the snap. |
| `name_template` | string | `<name>_<version>_<arch>.snap` | Output filename template. |
| `publish` | bool | `false` | Upload to the Snap Store after building. |
| `channel_templates` | list | | Store channels to release to (e.g. `edge`, `beta`, `stable`). |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the snap. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds (e.g. `{{ .CommitTimestamp }}`). |
| `disable` | bool | `false` | Skip this snapcraft config. |

### Confinement values

| Value | Description |
|-------|-------------|
| `strict` | Fully confined to declared interfaces (default, recommended for production). |
| `devmode` | Development mode — no confinement enforcement, useful for testing. |
| `classic` | Traditional package — no sandbox, requires Snap Store approval. |

### App config (`apps`)

Each entry under `apps` describes an application exposed by the snap.

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Command path relative to the snap root. |
| `args` | string | Additional arguments appended to the command. |
| `daemon` | string | Run as a daemon: `simple`, `forking`, `oneshot`, `notify`. |
| `stop_mode` | string | Signal used to stop the daemon: `sigterm`, `sigkill`, etc. |
| `restart_condition` | string | When to restart: `on-failure`, `always`, `never`, etc. |
| `plugs` | list | Interfaces this app needs. |
| `environment` | map | Environment variables for the app. |

When no `apps` map is provided, a default entry is generated using the first binary's name with the command `bin/<name>`.

### Layout config (`layouts`)

| Field | Type | Description |
|-------|------|-------------|
| `bind` | string | Bind-mount a host directory into the snap. |
| `symlink` | string | Create a symlink within the snap's layout. |

## Architecture mapping

Target triple components are mapped to Snapcraft architecture names:

| Rust target | Snap arch |
|-------------|-----------|
| `x86_64` / `amd64` | `amd64` |
| `aarch64` / `arm64` | `arm64` |
| `armv7` | `armhf` |
| `i686` / `i386` | `i386` |
| `s390x` | `s390x` |
| `ppc64le` | `ppc64el` |
| `riscv64` | `riscv64` |

## Publishing to the Snap Store

Set `publish: true` and authenticate with `snapcraft login` (or set `SNAPCRAFT_STORE_CREDENTIALS`) before running anodize. When `channel_templates` is provided, the snap is released to those channels automatically via `snapcraft upload --release`.

## Full example

```yaml
crates:
  - name: myapp
    snapcrafts:
      - name: myapp
        title: "My Application"
        summary: "A fast, cross-platform tool"
        description: |
          A longer description displayed in the Snap Store.
          Supports multiple paragraphs.
        base: core22
        grade: stable
        confinement: strict
        license: MIT
        plugs:
          - home
          - network
        apps:
          myapp:
            command: bin/myapp
            plugs:
              - home
              - network
        extra_files:
          - LICENSE
        publish: true
        channel_templates:
          - edge
          - stable
        replace: true
```
