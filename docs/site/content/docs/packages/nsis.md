+++
title = "NSIS"
description = "Create Windows installers with NSIS"
weight = 67
template = "docs.html"
+++

Anodizer can create Windows `.exe` installers using [NSIS (Nullsoft Scriptable Install System)](https://nsis.sourceforge.io/).

## Classification

Packager — creates Windows `.exe` installers from Windows binaries. Required: not a publisher; `makensis` must be on PATH.

## Minimal config

```yaml
crates:
  - name: myapp
    nsis:
      - {}
```

## NSIS config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier |
| `ids` | list | all builds | Filter by build IDs |
| `name` | string | `{{ ProjectName }}_{{ Version }}_{{ Arch }}_setup.exe` | Output installer filename (template) |
| `script` | string | built-in default | Path to a custom NSIS `.nsi` script (template) |
| `extra_files` | list | none | Additional files to include |
| `templated_extra_files` | list | none | Template-rendered extra files |
| `replace` | bool | `false` | Remove source archives, keeping only the installer |
| `mod_timestamp` | string | none | Reproducible build timestamp (template) |
| `disable` | string/bool | none | Disable this config |

## Prerequisites

The `makensis` command must be installed and available on PATH.

## Default script

When no custom `script` is provided, Anodizer uses a built-in NSIS script with:

- Modern UI 2 (MUI2) interface
- Install and uninstall sections
- Desktop shortcut creation
- Admin execution level
- Installation to `$PROGRAMFILES\{ProjectName}`
- Uninstaller registration

The default script uses these template variables: `{{ ProjectName }}`, `{{ NsisOutputFile }}`, `{{ NsisBinaryPath }}`, `{{ NsisBinaryName }}`.

## Custom script

Provide your own `.nsi` script for full control:

```yaml
crates:
  - name: myapp
    nsis:
      - script: installer/myapp.nsi
```

Custom scripts are rendered through the template engine, so all template variables are available.

## Authentication

Not applicable — NSIS installer creation is a local build step with no external service calls.

## Common gotchas

- **`makensis` must be on `PATH`**: install via `sudo apt-get install nsis` (Linux) or download from the NSIS website (Windows).
- **Windows only**: the stage ignores non-Windows binary artifacts.
- **Custom script template rendering**: custom `.nsi` scripts are rendered through Tera before being passed to `makensis`. Ensure any template expressions are valid.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Behavior

- Only processes Windows binary artifacts
- The `.exe` extension is auto-appended if not present in `name`
- Output is placed in `dist/windows/`
- `mod_timestamp` is applied to the staging directory and final output
- Skippable with `--skip nsis`

## Full config reference

```yaml
crates:
  - name: myapp
    nsis:
      - id: ""                        # optional; unique identifier
        ids: []                       # optional; filter by build IDs
        name: ""                      # optional; output installer filename template
        script: ""                    # optional; custom .nsi script (template)
        extra_files: []               # optional; additional files to include
        templated_extra_files: []     # optional; template-rendered extra files
        replace: false                # optional; remove archive artifacts, keep installer only
        mod_timestamp: ""             # optional; reproducible build timestamp
        disable: false                # optional
```

## Full example

```yaml
crates:
  - name: myapp
    nsis:
      - name: "MyApp_{{ Version }}_{{ Arch }}_setup.exe"
        extra_files:
          - LICENSE
          - README.md
        mod_timestamp: "{{ .CommitTimestamp }}"
```
