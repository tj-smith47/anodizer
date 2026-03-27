+++
title = "nFPM (deb/rpm/apk)"
description = "Generate Linux packages using nFPM"
weight = 3
template = "docs.html"
+++

Anodize integrates with [nFPM](https://nfpm.goreleaser.com/) to generate native Linux packages.

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
| `formats` | list | — | Package formats: `deb`, `rpm`, `apk` |
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
