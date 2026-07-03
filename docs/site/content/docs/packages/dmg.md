+++
title = "DMG"
description = "Create macOS disk image (.dmg) packages from Darwin binaries"
weight = 61
template = "docs.html"
+++

The DMG stage creates `.dmg` disk images from your macOS binaries. Images are placed in `dist/macos/`.

## Classification

Packager — creates macOS disk images from Darwin binaries. Required: not a publisher; macOS only.

## Minimal config

```yaml
crates:
  - name: myapp
    dmgs:
      - {}
```

## Full config reference

```yaml
crates:
  - name: myapp
    dmgs:
      - id: ""                        # optional; unique identifier
        ids: []                       # optional; filter by build IDs
        name: ""                      # optional; output filename template
        volume_name: ""               # optional; volume label (template); defaults to project name
        extra_files: []               # optional; additional files inside the disk image
        replace: false                # optional; remove archive artifacts, keep DMG only
        mod_timestamp: ""             # optional; fixed timestamp for reproducible builds
        use: binary                   # optional; "binary" (default) or "appbundle"
        amd64_variant: v1             # optional; amd64 variant filter
        if: ""                        # optional; template-conditional skip
        skip: false                   # optional
```

## Authentication

Not applicable — DMG creation is a local build step with no external service calls.

## Common gotchas

- **macOS only**: the stage ignores non-Darwin artifacts.
- **Tool selection**: `hdiutil` (macOS) is tried first, then `genisoimage`, then `mkisofs`. The stage fails if none is found.
- **Cross-compilation**: `genisoimage`/`mkisofs` allow Linux hosts to produce DMGs, but the resulting image may not be pixel-perfect compared to `hdiutil`-produced images.
- **`/Applications` symlink on Windows hosts**: when `use: appbundle`, anodizer inserts an `/Applications` symlink so mounted volumes present the standard drag-and-drop install UX. Due to how symbolic links are handled on Windows, this symlink may not work correctly if the image is built on a Windows host.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Required tools

One of the following must be available:

| Tool | Platform | Notes |
|------|----------|-------|
| `hdiutil` | macOS | Native; produces compressed UDZO images. Preferred. |
| `genisoimage` | Linux | Cross-compilation fallback. |
| `mkisofs` | Linux | Second fallback if `genisoimage` is absent. |

Tool selection is automatic: `hdiutil` is tried first, then `genisoimage`, then `mkisofs`. The stage fails if none is found.

## Platform

DMG only processes binary artifacts targeting Darwin (macOS). Binaries for other operating systems are ignored.

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `name` | string | `{{ ProjectName }}_{{ Arch }}` | Output filename template. |
| `volume_name` | string | project name | Volume label shown in Finder. Supports templates. |
| `extra_files` | list | | Additional files to include inside the disk image. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the DMG. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds. Supports templates (e.g. `{{ CommitTimestamp }}`). |
| `use` | string | `binary` | Which artifact type to package: `binary` or `appbundle`. |
| `amd64_variant` | enum | | amd64 microarchitecture variant filter — exactly one of `v1`/`v2`/`v3`/`v4` (any other value is rejected when the config is parsed). |
| `if` | string | | Template-conditional: skip this config when rendered result is falsy. |
| `skip` | bool/string | `false` | Skip this DMG config. Accepts bool or template string. |

## Volume name

The volume label is set to the project name by default. Override with `volume_name:` — it supports the same template variables as `name`.

```yaml
dmgs:
  - volume_name: "{{ ProjectName }} Installer"
```

## `/Applications` symlink

When `use: appbundle`, anodizer automatically inserts an `/Applications` symlink into the DMG's staging directory. This gives users the familiar drag-and-drop install UX when the disk image is opened in Finder.

```
MyApp.app  →  [drag here]  →  Applications (symlink → /Applications)
```

On Windows hosts the symlink may not resolve correctly — this matches GoReleaser's documented behavior.

## `extra_files` glob caveat

When a glob pattern matches more than one file, `name_template` cannot be used (it would overwrite every matched file to the same destination). anodizer bails with an error in this case. Use a more specific glob or omit `name_template` to preserve each file's original name.

```yaml
extra_files:
  - glob: "dist/readme-*.md"
    name_template: "README.md"   # error if glob matches > 1 file
```

## Background images and icon layout

anodizer does not currently support DMG background images or icon positioning. For polished Mac-style DMGs use the `before:`/`after:` hooks to drive `create-dmg` or `dmgbuild` directly.

## Template variables

The `name` and `volume_name` fields support standard template variables: `{{ ProjectName }}`, `{{ Version }}`, `{{ Arch }}`, `{{ Os }}`, `{{ Tag }}`.

## Full example

```yaml
crates:
  - name: myapp
    dmgs:
      - name: "{{ ProjectName }}_{{ Arch }}"
        volume_name: "{{ ProjectName }} Installer"
        extra_files:
          - LICENSE
          - README.md
        replace: true
        mod_timestamp: "{{ CommitTimestamp }}"
```

## Multiple configs

You can define several DMG configs per crate, for example to use `ids` filtering to build separate images for different binary variants:

```yaml
crates:
  - name: myapp
    dmgs:
      - ids: [myapp-amd64]
        name: "myapp_amd64"
      - ids: [myapp-arm64]
        name: "myapp_arm64"
```
