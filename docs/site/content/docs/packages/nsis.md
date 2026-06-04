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

## Authentication

Not applicable — NSIS installer creation is a local build step with no external service calls.

## Common gotchas

- **`makensis` must be on `PATH`**: install via `sudo apt-get install nsis` (Linux) or download from the NSIS website (Windows).
- **Windows only**: the stage ignores non-Windows binary artifacts.
- **Custom script template rendering**: custom `.nsi` scripts are rendered through Tera before being passed to `makensis`. Ensure any template expressions are valid Tera syntax — Go's `text/template` directives (e.g. `{{ range }}`) do not port directly.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## NSIS config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier |
| `ids` | list | all builds | Filter by build IDs |
| `name` | string | `{{ ProjectName }}_{{ Arch }}_setup` | Output installer filename (template). Include `.exe` yourself, or rely on the built-in script's `OutFile "{{ Name }}.exe"`. |
| `script` | string | built-in default | Path to a custom NSIS `.nsi` script (template) |
| `extra_files` | list | none | Additional files to include (glob, or `{glob, name_template}`) |
| `templated_extra_files` | list | none | Template-rendered extra files |
| `replace` | bool | `false` | Remove source archives, keeping only the installer |
| `mod_timestamp` | string | none | Reproducible build timestamp (template) |
| `disable` | string/bool | none | Disable this config |

## Prerequisites

The `makensis` command must be installed and available on PATH.

## Defaults vs GoReleaser

Anodizer ships a built-in NSIS script so a minimal `nsis: [ {} ]` config produces a working installer out of the box. GoReleaser Pro requires `script:` to be set explicitly. Set `script:` yourself for behaviour identical to an upstream GoReleaser config.

The `name` default also differs: anodizer matches GoReleaser's `'{{ProjectName}}_{{Arch}}_setup'` and does **not** auto-append `.exe`. The built-in default script writes `OutFile "{{ Name }}.exe"`, so the resulting installer is `.exe`-suffixed even though the `name` template is not. Custom scripts must include `.exe` in their `OutFile` directive themselves.

## Template engine

NSIS scripts go through the [Tera](https://keats.github.io/tera/) template engine, not Go's `text/template`. Most simple variable substitutions port directly, but more complex constructs do not — Go's `{{ range $i, $v := .Files }}…{{ end }}` becomes Tera's `{% for v in Files %}…{% endfor %}`. Imported GoReleaser scripts that use control flow will need a port.

## Template variables

| Variable | Value | Notes |
|----------|-------|-------|
| `{{ ProjectName }}` | configured project name | from top-level config |
| `{{ Version }}`, `{{ Tag }}`, etc. | release metadata | standard anodizer vars |
| `{{ Os }}`, `{{ Target }}` | binary target metadata | global vars |
| `{{ Arch }}` | NSIS-native arch (`x86`, `x64`, `arm64`) | overridden only inside the NSIS render context — global `Arch` retains the Go-style value (`amd64`, `386`, `arm64`) for other stages |
| `{{ Name }}` | rendered output stem (the `name` template result) | use as `OutFile "{{ Name }}.exe"` |
| `{{ ProgramFiles }}` | `$PROGRAMFILES64` for 64-bit (`x64`, `arm64`), `$PROGRAMFILES` for 32-bit | use as `InstallDir "{{ ProgramFiles }}\YourApp"` to avoid the WOW6432-redirected `Program Files (x86)` path on 64-bit Windows |
| `{{ Binary }}` | binary filename (with `.exe`) | use as `File "{{ Binary }}"` |
| `{{ NsisOutputFile }}` | absolute path to the rendered output | anodizer-specific |
| `{{ NsisBinaryPath }}` | absolute path to the staged binary | anodizer-specific |
| `{{ NsisBinaryName }}` | binary filename | anodizer-specific |

## Default script

When no custom `script` is provided, anodizer uses a built-in NSIS script with:

- Modern UI 2 (MUI2) interface
- Install and uninstall sections
- Desktop shortcut creation
- Admin execution level (`RequestExecutionLevel admin`)
- Installation to `{{ ProgramFiles }}\{{ ProjectName }}` — arch-aware (`$PROGRAMFILES64` on 64-bit targets, `$PROGRAMFILES` on 32-bit)
- Uninstaller registration

## Custom script

Provide your own `.nsi` script for full control:

```yaml
crates:
  - name: myapp
    nsis:
      - script: installer/myapp.nsi
```

Custom scripts are rendered through the template engine, so all template variables documented above are available.

## `extra_files` glob caveat

When an `extra_files` entry uses the `{glob, name_template}` form, `name_template` is only valid when the glob matches **exactly one file**. A multi-match glob paired with a constant `name_template` would silently overwrite every match to the same destination name, so anodizer bails up-front in that case. Use a bare glob string (or one entry per file) when copying multiple matches.

```yaml
# Valid — single-match glob renamed
extra_files:
  - glob: "vendor/single.dll"
    name_template: "myapp.dll"

# Valid — multi-match glob, no renaming
extra_files:
  - glob: "vendor/*.dll"

# Invalid — bails at stage-run time
extra_files:
  - glob: "vendor/*.dll"
    name_template: "myapp.dll"
```

## Signing

NSIS installers should be Authenticode-signed before distribution so Windows SmartScreen does not flag them. Anodizer (matching GoReleaser) does not run `signtool` automatically; sign after the NSIS stage via an `after:` hook:

```yaml
hooks:
  after:
    - cmd: signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /f cert.pfx /p $CERT_PASSWORD dist/windows/*.exe
      env:
        - CERT_PASSWORD={{ Env.WINDOWS_CERT_PASSWORD }}
```

On Linux/macOS hosts, [`osslsigncode`](https://github.com/mtrojnar/osslsigncode) is the standard `signtool` replacement.

## Behavior

- Only processes Windows binary artifacts
- The `name` template is taken verbatim — no auto `.exe` append
- Output is placed in `dist/windows/`
- `mod_timestamp` is applied to the staging directory and final output
- Skippable with `--skip nsis`

## Full example

```yaml
crates:
  - name: myapp
    nsis:
      - name: "MyApp_{{ Version }}_{{ Arch }}_setup.exe"
        extra_files:
          - LICENSE
          - README.md
        mod_timestamp: "{{ CommitTimestamp }}"
```
