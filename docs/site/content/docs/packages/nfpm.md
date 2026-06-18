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

## Full config reference

```yaml
crates:
  - name: myapp
    nfpm:
      - package_name: myapp            # optional; defaults to crate name
        formats: [deb, rpm]            # REQUIRED (>= 1 entry): deb | rpm | apk | archlinux | ipk
                                       #   with no formats entry, this config emits nothing
        vendor: ""                     # optional
        homepage: ""                   # optional
        maintainer: ""                 # optional; maintainer email
        description: ""                # optional
        license: ""                    # optional; SPDX identifier
        bindir: /usr/bin               # optional; binary installation directory
        bin_alias: ""                  # optional; rename the installed binary inside the package only
        file_name_template: ""         # optional; custom filename template
        contents: []                   # optional; additional files to include
        dependencies: {}               # optional; per-format package dependencies
        recommends: []                 # optional; soft dependencies
        suggests: []                   # optional; weaker-than-recommends dependencies
        conflicts: []                  # optional; packages this conflicts with
        replaces: []                   # optional; packages this replaces (rename upgrades)
        provides: []                   # optional; virtual packages provided
        scripts:                       # optional; install scripts
          preinstall: ""
          postinstall: ""
          preremove: ""
          postremove: ""
        overrides: {}                  # optional; per-format field overrides
        rpm: {}                        # optional; RPM-specific block (see Per-format blocks)
        deb: {}                        # optional; deb-specific block
        apk: {}                        # optional; apk-specific block
        archlinux: {}                  # optional; archlinux-specific block
        ipk: {}                        # optional; ipk (OpenWrt) block
```

## Authentication

Not applicable — nFPM generates package files locally. Uploading them to a package repository is handled by the cloudsmith, gemfury, or artifactory publishers.

## Common gotchas

- **`formats`**: the list must match the package types your downstream publishers expect. A mismatch (e.g., configuring cloudsmith for `deb` but nFPM only producing `rpm`) silently skips upload.
- **`dependencies`**: per-format dependency maps allow different deps for deb vs rpm — use the `overrides` map for format-specific fields.
- **Linux only**: nFPM only processes Linux binary artifacts. Darwin/Windows targets are ignored.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## nFPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `package_name` | string | crate name | Package name |
| `formats` | list | **required** | Package formats: `deb`, `rpm`, `apk`, `archlinux`, `ipk`. At least one entry is required — a config with no `formats` produces no package. |
| `vendor` | string | Cargo first author | Distributing entity recorded in the rpm/deb `Vendor` field. Auto-derives from the crate's first `Cargo.toml` author (with any `<email>` suffix stripped); set to override. See [Vendor](#vendor). |
| `homepage` | string | none | Homepage URL |
| `maintainer` | string | none | Maintainer email |
| `description` | string | none | Package description |
| `license` | string | none | License identifier |
| `bindir` | string | `/usr/bin` | Binary installation directory |
| `bin_alias` | string | none | Rename the installed binary inside the package only (e.g. `fd` → `fdfind` for the Debian package); the build output and archive are untouched. Templated. |
| `file_name_template` | string | auto | Custom filename template |
| `contents` | list | none | Additional files to include |
| `dependencies` | map | none | Package dependencies keyed by format (e.g., `deb: [git]`) |
| `recommends` | list | none | Soft (recommended) dependencies |
| `suggests` | list | none | Suggested dependencies (weaker than `recommends`) |
| `conflicts` | list | none | Packages this package conflicts with |
| `replaces` | list | none | Packages this package replaces (upgrade paths from renamed packages) |
| `provides` | list | none | Virtual packages this package provides |
| `scripts` | object | none | Pre/post install/remove scripts (`preinstall`, `postinstall`, `preremove`, `postremove`) |
| `overrides` | map | none | Per-format field overrides |
| `rpm` | object | none | RPM-specific block (see [Per-format blocks](#per-format-blocks)) |
| `deb` | object | none | Deb-specific block |
| `apk` | object | none | APK-specific block |
| `archlinux` | object | none | Archlinux-specific block |
| `ipk` | object | none | IPK (OpenWrt) block |

## Vendor

The `Vendor` field of a deb/rpm package names the distributing entity. anodizer
auto-derives it from the crate's first `Cargo.toml` author, stripping any
`<email>` suffix — so a crate authored by `TJ Smith <tj@jarvispro.io>` produces
`Vendor: TJ Smith` without any nfpm config. Set `vendor:` only to override:

```toml
# Cargo.toml
[package]
authors = ["TJ Smith <tj@jarvispro.io>"]
```

```yaml
nfpm:
  - package_name: myapp
    formats: [deb, rpm]
    # vendor omitted -> derived as "TJ Smith"
```

`vendor: "Some Other Org"` overrides the derived value.

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

## Per-format blocks

Each package format has an optional dedicated block carrying settings that only
apply to that format. Set the block alongside `formats`; anodizer emits the
matching nfpm section only for the formats you list.

```yaml
nfpm:
  - package_name: myapp
    formats: [rpm, deb, apk, archlinux, ipk]
    rpm:
      summary: "A fast CLI tool"        # RPM Summary tag
      compression: zstd                 # lzma | gzip | xz | zstd
      group: "System/Tools"
      packager: "Build Team <build@example.com>"
      prefixes: ["/usr"]                # relocatable RPM prefixes
      build_host: "reproducible"        # override RPM BuildHost tag
      signature:
        key_file: signing.gpg
        key_passphrase: ""              # falls back to NFPM_PASSPHRASE
      scripts:
        pretrans: scripts/pretrans.sh   # %pretrans scriptlet
        posttrans: scripts/posttrans.sh # %posttrans scriptlet
    deb:
      compression: xz                   # gzip | xz | zstd | none
      predepends: ["dpkg (>= 1.17)"]    # stronger than Depends
      breaks: ["oldpkg (<< 2.0)"]       # Breaks relationship
      lintian_overrides: ["binary-without-manpage"]
      fields:                           # extra control fields
        Built-Using: "rustc"
      signature:
        key_file: signing.gpg
        type: origin                    # origin | maint | archive
      triggers:
        activate_noawait: ["ldconfig"]
      scripts:
        rules: debian/rules
        templates: debian/templates     # debconf templates
        config: debian/config           # debconf config script
    apk:
      signature:
        key_file: signing.rsa
        key_name: "build@example.com.rsa.pub"
      scripts:
        preupgrade: scripts/preupgrade.sh
        postupgrade: scripts/postupgrade.sh
    archlinux:
      pkgbase: myapp                    # base name for split packages
      packager: "Build Team <build@example.com>"
      scripts:
        preupgrade: scripts/preupgrade.sh
        postupgrade: scripts/postupgrade.sh
    ipk:
      abi_version: "1"
      auto_installed: false
      essential: false
      predepends: ["libc"]
      tags: ["net"]
      fields:
        Maintainer: "team@example.com"
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
