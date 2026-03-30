+++
title = "DMG"
description = "Create macOS disk image (.dmg) packages from Darwin binaries"
weight = 61
template = "docs.html"
+++

The DMG stage creates `.dmg` disk images from your macOS binaries. Images are placed in `dist/macos/`.

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

## Minimal config

```yaml
crates:
  - name: myapp
    dmgs:
      - {}
```

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `name` | string | `{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg` | Output filename template. |
| `extra_files` | list | | Additional files to include inside the disk image. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the DMG. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds (e.g. `{{ .CommitTimestamp }}`). |
| `disable` | bool | `false` | Skip this DMG config. |

## Volume name

The volume label is set to the project name. This is the name the user sees when they mount the image in Finder.

## Template variables

The `name` field supports standard template variables: `{{ ProjectName }}`, `{{ Version }}`, `{{ Arch }}`, `{{ Os }}`, `{{ Tag }}`.

## Full example

```yaml
crates:
  - name: myapp
    dmgs:
      - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg"
        extra_files:
          - LICENSE
          - README.md
        replace: true
        mod_timestamp: "{{ .CommitTimestamp }}"
```

## Multiple configs

You can define several DMG configs per crate, for example to use `ids` filtering to build separate images for different binary variants:

```yaml
crates:
  - name: myapp
    dmgs:
      - ids: [myapp-amd64]
        name: "myapp_{{ Version }}_amd64.dmg"
      - ids: [myapp-arm64]
        name: "myapp_{{ Version }}_arm64.dmg"
```
