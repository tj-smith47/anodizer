+++
title = "Flatpak"
description = "Build Flatpak bundles for Linux distribution"
weight = 65
template = "docs.html"
+++

Anodizer can build Flatpak bundles from your compiled Linux binaries using `flatpak-builder`.

## Classification

Packager — builds Flatpak bundles from Linux binaries. Required: not a publisher; `flatpak-builder` must be on PATH.

## Minimal config

```yaml
crates:
  - name: myapp
    flatpaks:
      - app_id: com.myorg.myapp
        runtime: org.freedesktop.Platform
        runtime_version: "24.08"
        sdk: org.freedesktop.Sdk
```

## Full config reference

```yaml
crates:
  - name: myapp
    flatpaks:
      - app_id: com.myorg.myapp      # required; reverse-DNS app ID
        runtime: org.freedesktop.Platform  # required
        runtime_version: "24.08"      # required
        sdk: org.freedesktop.Sdk      # required
        id: ""                        # optional; unique identifier
        ids: []                       # optional; filter by build IDs
        name_template: ""             # optional; output filename template
        command: ""                   # optional; command to run (default: first binary)
        finish_args: []               # optional; sandbox permissions
        extra_files: []               # optional; additional files to include
        replace: false                # optional; remove archive artifacts, keep Flatpak only
        mod_timestamp: ""             # optional; reproducible build timestamp
        disable: false                # optional
```

## Authentication

Not applicable — Flatpak bundle creation is a local build step. Publishing to Flathub requires a separate submission process not managed by anodizer.

## Common gotchas

- **`flatpak-builder` and runtime must be installed**: the stage errors if either is absent. Install runtimes with `flatpak install org.freedesktop.Platform//24.08`.
- **Linux x86_64/aarch64 only**: other architectures are skipped silently.
- **Flathub submission**: anodizer builds the `.flatpak` bundle but does not submit to Flathub. Submit manually or via a CI step after the release.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Flatpak config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier |
| `ids` | list | all builds | Filter by build IDs |
| `name_template` | string | `{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.flatpak` | Output filename (template) |
| `app_id` | string | **required** | Flatpak app ID (reverse-DNS, e.g., `com.myorg.myapp`) |
| `runtime` | string | **required** | Flatpak runtime (e.g., `org.freedesktop.Platform`) |
| `runtime_version` | string | **required** | Runtime version (e.g., `"24.08"`) |
| `sdk` | string | **required** | Flatpak SDK (e.g., `org.freedesktop.Sdk`) |
| `command` | string | first binary | Command to run inside the sandbox |
| `finish_args` | list | none | Sandbox permissions (e.g., `--share=network`) |
| `extra_files` | list | none | Additional files to include |
| `replace` | bool | `false` | Remove source archives, keeping only the Flatpak |
| `mod_timestamp` | string | none | Reproducible build timestamp (template) |
| `disable` | string/bool | none | Disable this config |

## Prerequisites

- `flatpak-builder` and `flatpak` must be installed
- The specified runtime and SDK must be available (`flatpak install`)

## Behavior

- Only processes Linux binary artifacts
- Supports x86_64 and aarch64 architectures (others are skipped)
- Generates a Flatpak manifest JSON with `buildsystem: "simple"`
- Extra files are installed to `/app/share/{app_id}/`
- Output is placed in `dist/flatpak/`
- Skippable with `--skip flatpak`

## Sandbox permissions

Use `finish_args` to grant sandbox permissions:

```yaml
flatpaks:
  - app_id: com.myorg.myapp
    runtime: org.freedesktop.Platform
    runtime_version: "24.08"
    sdk: org.freedesktop.Sdk
    finish_args:
      - "--share=network"
      - "--share=ipc"
      - "--socket=x11"
      - "--filesystem=home"
```

## Full example

```yaml
crates:
  - name: myapp
    flatpaks:
      - app_id: com.myorg.myapp
        runtime: org.freedesktop.Platform
        runtime_version: "24.08"
        sdk: org.freedesktop.Sdk
        command: myapp
        finish_args:
          - "--share=network"
        extra_files:
          - LICENSE
        mod_timestamp: "{{ CommitTimestamp }}"
```
