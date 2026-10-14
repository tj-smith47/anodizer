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

The block is the top-level `srpms:` key. The singular `srpm:` spelling is
still accepted.

```yaml
srpms:
  enabled: true
```

## Full config reference

```yaml
srpms:
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
    key_passphrase: ""               # optional; falls back to SRPM_PASSPHRASE env var
  bins:                              # optional; binary → owned install path (default %{_bindir}/<name>)
    myapp: /usr/bin/myapp
  prefixes: [/usr]                   # optional; one Prefix: tag each (relocatable package)
  build_host: reproducible           # optional; BuildHost: tag
  pretrans: scripts/pretrans.sh      # optional; %pretrans script, sourced on the installing host
  posttrans: scripts/posttrans.sh    # optional; %posttrans script, sourced on the installing host
  prerelease: rc1                    # optional; appended to Version: as ~rc1
  version_metadata: g1234abc         # optional; appended to Version: as +g1234abc
  skip: false                        # optional
```

## Authentication

| Variable | Description |
|----------|-------------|
| `SRPM_PASSPHRASE` | GPG passphrase for signing (or set `signature.key_passphrase` in config) |

No authentication is required when `signature` is not configured.

## Common gotchas

- **`rpmbuild` must be on `PATH`**: install via `sudo dnf install rpm-build` or `sudo apt-get install rpm`.
- **Exactly one source archive required**: the stage errors if zero or more than one source archive artifact exists. Ensure the archive stage runs first with a compatible format.
- **Signing**: `signature` requires a GPG key on the build machine. Ensure the key is imported before releasing.

## Republish / update behavior

Not applicable — this is a local packaging stage with no remote to publish to.

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
| `bins` | map | `%{_bindir}/<name>` per built binary | Binary name → install path owned in the spec's `%files` section; set it to move a binary or declare extra owned paths |
| `prefixes` | list | none | Filesystem prefixes the package may relocate to, one `Prefix:` tag each |
| `build_host` | string | what rpmbuild records | `BuildHost:` tag written into the spec header |
| `pretrans` | string | none | Path to a `%pretrans` script, sourced on the installing host before any package in the transaction installs (it must exist there) |
| `posttrans` | string | none | Path to a `%posttrans` script, sourced on the installing host after every package in the transaction installed (it must exist there) |
| `prerelease` | string | none | Prerelease component of the package version (e.g. `rc1`), appended to the generated spec's `Version:` after a `~` |
| `version_metadata` | string | none | Build-metadata component of the package version (e.g. a commit hash), appended to the generated spec's `Version:` after a `+` |
| `skip` | string/bool | none | Skip this config (accepts bool or template string) |

### Signature config

| Field | Type | Description |
|-------|------|-------------|
| `key_file` | string | Path to GPG key file |
| `key_passphrase` | string | GPG passphrase (falls back to `SRPM_PASSPHRASE` env var). |

## Prerequisites

- `rpmbuild` must be installed and available on PATH
- Exactly one source archive artifact must exist (from the archive stage with `format: tar.gz` or similar)

## Auto-generated spec

When no `spec_file` is provided, Anodizer generates a minimal RPM spec with `%autosetup`, `%build`, `%install`, `%files`, and `%changelog` sections.

The `%prep` step is prefix-aware: it emits `%autosetup -n {{ SourcePrefix }}` to enter the source archive's top-level directory, or `%autosetup -c` when the archive has no prefix directory (`source.prefix_template` unset). This keeps the produced `.src.rpm` rebuildable for snapshot and prerelease versions, whose RPM `Version:` is sanitized but whose tarball prefix is the raw version.

## Custom spec file

Provide your own `.spec` template for full control:

```yaml
srpms:
  enabled: true
  spec_file: rpm/myapp.spec
```

The spec file is rendered through the template engine with additional variables:

| Variable | Description |
|----------|-------------|
| `{{ PackageName }}` | RPM package name |
| `{{ Source }}` | Source archive filename (reconciled with the RPM `Version:`; always matches the file in `SOURCES/`) |
| `{{ SourcePrefix }}` | Source archive top-level directory (empty for a flat archive) |
| `{{ Summary }}` | Package summary |
| `{{ License }}` | License identifier |
| `{{ URL }}` | Homepage URL |
| `{{ Description }}` | Package description |
| `{{ Bins }}` | Map of binary name → owned install path (`bins:` or the `%{_bindir}/<name>` default) |
| `{{ Prefixes }}` | One `Prefix: <path>` line per `prefixes` entry |
| `{{ BuildHost }}` | `build_host` |
| `{{ Pretrans }}` / `{{ Posttrans }}` | `pretrans` / `posttrans` script paths |
| `{{ Prerelease }}` / `{{ VersionMetadata }}` | `prerelease` / `version_metadata` |

Use `%autosetup -n {{ SourcePrefix }}` only when the source archive has a
directory prefix (a `source.prefix_template` ending in `/`). For a flat
archive (`SourcePrefix` empty), use `%autosetup -c` instead, which creates
the build directory and extracts the flat sources into it.

## Relocation, scriptlets and version components

```yaml
srpms:
  enabled: true
  prefixes: [/usr, /etc]
  build_host: reproducible
  pretrans: /usr/share/myapp/pretrans.sh
  posttrans: /usr/share/myapp/posttrans.sh
  prerelease: rc1
  version_metadata: g1234abc
```

For version `1.2.0` the generated spec's `Version:` line reads:

```text
Version:        1.2.0~rc1+g1234abc
```

the header, after `Source0:`, carries:

```text
BuildHost:      reproducible
Prefix:         /usr
Prefix:         /etc
```

and the scriptlets follow the `%files` section:

```text
%pretrans
. /usr/share/myapp/pretrans.sh

%posttrans
. /usr/share/myapp/posttrans.sh
```

The `Version:` value is made RPM-safe after the parts are joined, so a `-` in
the version, `prerelease` or `version_metadata` never reaches rpmbuild. The
scriptlets source each path with `.` when the package installs, so the script
must already exist on the installing host; anodizer does not add it to the
package. A `bins` entry replaces the default `%{_bindir}/<name>` path for that
binary in `%files`:

```yaml
srpms:
  enabled: true
  bins:
    myapp: /opt/myapp/bin/myapp
```

## Behavior

- The `.src.rpm` extension is auto-appended if not present
- Respects global `--skip-sign` — signature config is cleared when signing is skipped
- Skippable with `--skip srpm`

## Full example

```yaml
srpms:
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
