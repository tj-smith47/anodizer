+++
title = "macOS App Bundles"
description = "Create macOS .app bundles from your compiled binaries"
weight = 64
template = "docs.html"
+++

Anodizer can package your macOS binaries into `.app` bundles with a proper directory structure, `Info.plist`, and optional icon.

## Classification

Packager — creates macOS `.app` bundle directories from Darwin binaries. Required: not a publisher; runs only on macOS targets.

## Minimal config

```yaml
crates:
  - name: myapp
    app_bundles:
      - bundle: com.myorg.myapp
```

## App bundle config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier for this config |
| `ids` | list | all builds | Filter by build IDs |
| `name` | string | `{{ ProjectName }}_{{ Arch }}.app` | Output .app bundle name (template) |
| `icon` | string | none | Path to `.icns` icon file (template) |
| `bundle` | string | **required** | Bundle identifier in reverse-DNS notation |
| `extra_files` | list | none | Additional files to include in Resources |
| `templated_extra_files` | list | none | Template-rendered extra files |
| `mod_timestamp` | string | none | Override mtime for reproducible builds (template) |
| `replace` | bool | `false` | Remove source archives, keeping only the app bundle |
| `disable` | string/bool | none | Disable this config (bool or template) |

## Generated structure

The stage creates a standard macOS `.app` directory:

```
MyApp.app/
  Contents/
    Info.plist
    MacOS/
      myapp          (binary, chmod 755)
    Resources/
      icon.icns      (if configured)
      ...            (extra files)
```

## Info.plist

Anodizer auto-generates `Info.plist` with:

| Key | Value |
|-----|-------|
| `CFBundleExecutable` | Binary name |
| `CFBundleIdentifier` | `bundle` field value |
| `CFBundleName` | Project name |
| `CFBundleVersion` | Version |
| `CFBundleShortVersionString` | Version |
| `CFBundlePackageType` | `APPL` |
| `CFBundleInfoDictionaryVersion` | `6.0` |
| `NSHighResolutionCapable` | `true` |
| `LSMinimumSystemVersion` | `10.13` |
| `CFBundleIconFile` | Icon filename (when `icon` is set) |

## Authentication

Not applicable — app bundle creation is a local build step with no external service calls.

## Common gotchas

- **macOS only**: the stage only processes Darwin binary artifacts. Non-macOS targets are ignored.
- **`.app` extension**: auto-appended if not present in `name`.
- **`icon` must be `.icns`**: other formats (`.png`, `.svg`) are not accepted by macOS for the `CFBundleIconFile` key.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Behavior

- Only processes macOS (darwin) binary artifacts
- The `.app` extension is auto-appended if not present in `name`
- Output is placed in `dist/macos/`
- `mod_timestamp` is applied recursively to the entire `.app` tree
- Skippable with `--skip appbundle`

## Full config reference

```yaml
crates:
  - name: myapp
    app_bundles:
      - bundle: com.example.myapp    # required; reverse-DNS bundle identifier
        id: ""                        # optional; unique identifier
        ids: []                       # optional; filter by build IDs
        name: ""                      # optional; output .app name template
        icon: ""                      # optional; path to .icns icon file (template)
        extra_files: []               # optional; additional files to include in Resources
        templated_extra_files: []     # optional; template-rendered extra files
        mod_timestamp: ""             # optional; override mtime for reproducible builds
        replace: false                # optional; remove source archives, keep app bundle
        disable: false                # optional; bool or template string
```

## Full example

```yaml
crates:
  - name: myapp
    app_bundles:
      - name: "MyApp_{{ Arch }}.app"
        bundle: com.myorg.myapp
        icon: assets/icon.icns
        extra_files:
          - LICENSE
          - README.md
        mod_timestamp: "{{ .CommitTimestamp }}"
        replace: true
```
