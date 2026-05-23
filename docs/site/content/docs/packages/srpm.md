+++
title = "Source RPM"
description = "Build source RPMs (.src.rpm) from your project"
weight = 68
template = "docs.html"
+++

Anodizer can build source RPM packages (`.src.rpm`) using `rpmbuild`.

## Classification

Packager — creates source RPM packages from your project. Required: not a publisher; disabled by default.

## Minimal config

```yaml
srpm:
  enabled: true
```

## Full config reference

```yaml
srpm:
  enabled: false                     # required; opt-in (disabled by default)
  package_name: ""                   # optional; RPM package name (default: project name)
  file_name_template: ""             # optional; output filename template
  spec_file: ""                      # optional; path to .spec template (auto-generated if omitted)
  epoch: ""                          # optional; RPM epoch
  section: ""                        # optional; RPM section
  maintainer: ""                     # optional
  vendor: ""                         # optional
  summary: ""                        # optional
  group: ""                          # optional
  description: ""                    # optional
  license: MIT                       # optional; license identifier
  license_file_name: ""              # optional; license file to include
  url: ""                            # optional; homepage URL
  packager: ""                       # optional; RPM packager field
  compression: ""                    # optional; gzip | xz | zstd | none
  docs: []                           # optional; documentation files to include
  contents: []                       # optional; additional contents
  signature:                         # optional; RPM signing config
    key_file: ""
    passphrase: ""                   # optional; falls back to SRPM_PASSPHRASE env var
  disable: false                     # optional
```

## Authentication

| Variable | Description |
|----------|-------------|
| `SRPM_PASSPHRASE` | GPG passphrase for signing (or set `signature.passphrase` in config) |

No authentication is required when `signature` is not configured.

## Common gotchas

- **`rpmbuild` must be on `PATH`**: install via `sudo dnf install rpm-build` or `sudo apt-get install rpm`.
- **Exactly one source archive required**: the stage errors if zero or more than one source archive artifact exists. Ensure the archive stage runs first with a compatible format.
- **Signing**: `signature` requires a GPG key on the build machine. Ensure the key is imported before releasing.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## SRPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable SRPM generation |
| `package_name` | string | project name | RPM package name |
| `file_name_template` | string | `{{ PackageName }}-{{ Version }}.src.rpm` | Output filename (template) |
| `spec_file` | string | auto-generated | Path to RPM spec file template |
| `epoch` | string | none | RPM epoch |
| `section` | string | none | RPM section |
| `maintainer` | string | package name | Package maintainer |
| `vendor` | string | none | Package vendor |
| `summary` | string | package name | Summary line |
| `group` | string | none | RPM group |
| `description` | string | package name | Package description |
| `license` | string | `MIT` | License identifier |
| `license_file_name` | string | none | License file to include |
| `url` | string | `""` | Homepage URL |
| `packager` | string | none | RPM packager field |
| `compression` | string | none | Compression: `gzip`, `xz`, `zstd`, `none` |
| `docs` | list | none | Documentation files to include |
| `contents` | list | none | Additional contents (same format as nFPM contents) |
| `signature` | object | none | RPM signing configuration |
| `disable` | string/bool | none | Disable this config |

### Signature config

| Field | Type | Description |
|-------|------|-------------|
| `key_file` | string | Path to GPG key file |
| `passphrase` | string | GPG passphrase (falls back to `SRPM_PASSPHRASE` env var) |

## Prerequisites

- `rpmbuild` must be installed and available on PATH
- Exactly one source archive artifact must exist (from the archive stage with `format: tar.gz` or similar)

## Auto-generated spec

When no `spec_file` is provided, Anodizer generates a minimal RPM spec with `%autosetup`, `%build`, `%install`, `%files`, and `%changelog` sections.

## Custom spec file

Provide your own `.spec` template for full control:

```yaml
srpm:
  enabled: true
  spec_file: rpm/myapp.spec
```

The spec file is rendered through the template engine with additional variables:

| Variable | Description |
|----------|-------------|
| `{{ .PackageName }}` | RPM package name |
| `{{ .Source }}` | Source archive filename |
| `{{ .Summary }}` | Package summary |
| `{{ .License }}` | License identifier |
| `{{ .URL }}` | Homepage URL |
| `{{ .Description }}` | Package description |

## Behavior

- The `.src.rpm` extension is auto-appended if not present
- Respects global `--skip-sign` — signature config is cleared when signing is skipped
- Skippable with `--skip srpm`

## Full example

```yaml
srpm:
  enabled: true
  package_name: myapp
  summary: "A fast CLI tool"
  description: "My application does great things"
  license: Apache-2.0
  url: "https://example.com/myapp"
  vendor: "My Org"
  maintainer: "Alice <alice@example.com>"
  compression: xz
  docs:
    - README.md
    - CHANGELOG.md
  signature:
    key_file: /path/to/gpg-key.asc
```
