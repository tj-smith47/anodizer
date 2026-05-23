+++
title = "nFPM (deb/rpm/apk)"
description = "Generate Linux packages using nFPM"
weight = 3
template = "docs.html"
+++

Anodizer integrates with [nFPM](https://nfpm.goreleaser.com/) to generate native Linux packages.

## Classification

Packager — generates deb/rpm/apk/archlinux/ipk packages from Linux binaries. Required: not a publisher; always runs unless disabled.

## Minimal config

```yaml
crates:
  - name: myapp
    nfpm:
      - package_name: myapp
        formats: [deb, rpm]
        vendor: "My Company"
        homepage: "https://example.com"
        maintainer: "maintainer@example.com"
        description: "My application"
        license: MIT
```

## nFPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `package_name` | string | — | Package name |
| `formats` | list | — | Package formats: `deb`, `rpm`, `apk`, `archlinux`, `ipk` |
| `vendor` | string | none | Package vendor |
| `homepage` | string | none | Homepage URL |
| `maintainer` | string | none | Maintainer email |
| `description` | string | none | Package description |
| `license` | string | none | License identifier |
| `bindir` | string | `/usr/bin` | Binary installation directory |
| `file_name_template` | string | auto | Custom filename template |
| `contents` | list | none | Additional files to include |
| `dependencies` | map | none | Package dependencies keyed by format (e.g., `deb: [git]`) |
| `scripts` | object | none | Pre/post install/remove scripts |
| `overrides` | map | none | Per-format field overrides |

## File contents

Include additional files in the package:

```yaml
nfpm:
  - package_name: myapp
    formats: [deb, rpm]
    contents:
      - src: config.example.yaml
        dst: /etc/myapp/config.yaml
        type: config
      - src: myapp.service
        dst: /usr/lib/systemd/system/myapp.service
```

## Install scripts

```yaml
nfpm:
  - package_name: myapp
    formats: [deb]
    scripts:
      preinstall: scripts/preinstall.sh
      postinstall: scripts/postinstall.sh
      preremove: scripts/preremove.sh
      postremove: scripts/postremove.sh
```

## Authentication

Not applicable — nFPM generates package files locally. Uploading them to a package repository is handled by the cloudsmith, gemfury, or artifactory publishers.

## Common gotchas

- **`formats`**: the list must match the package types your downstream publishers expect. A mismatch (e.g., configuring cloudsmith for `deb` but nFPM only producing `rpm`) silently skips upload.
- **`dependencies`**: per-format dependency maps allow different deps for deb vs rpm — use the `overrides` map for format-specific fields.
- **Linux only**: nFPM only processes Linux binary artifacts. Darwin/Windows targets are ignored.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Full config reference

```yaml
crates:
  - name: myapp
    nfpm:
      - package_name: myapp            # optional; defaults to crate name
        formats: [deb, rpm]            # optional; deb | rpm | apk | archlinux | ipk
        vendor: ""                     # optional
        homepage: ""                   # optional
        maintainer: ""                 # optional; maintainer email
        description: ""                # optional
        license: ""                    # optional; SPDX identifier
        bindir: /usr/bin               # optional; binary installation directory
        file_name_template: ""         # optional; custom filename template
        contents: []                   # optional; additional files to include
        dependencies: {}               # optional; per-format package dependencies
        scripts:                       # optional; install scripts
          preinstall: ""
          postinstall: ""
          preremove: ""
          postremove: ""
        overrides: {}                  # optional; per-format field overrides
```

## Full example

```yaml
crates:
  - name: myapp
    nfpm:
      - package_name: myapp
        formats: [deb, rpm, apk]
        vendor: "My Company"
        homepage: "https://github.com/myorg/myapp"
        maintainer: "team@example.com"
        description: "A fast CLI tool"
        license: MIT
        dependencies:
          deb:
            - git
          rpm:
            - git
        contents:
          - src: config.example.yaml
            dst: /etc/myapp/config.yaml
            type: config
        overrides:
          deb:
            dependencies:
              - git
              - ca-certificates
```
