+++
title = "macOS PKG"
description = "Build macOS installer packages using pkgbuild"
weight = 63
template = "docs.html"
+++

The PKG stage builds macOS `.pkg` installer packages from your Darwin binaries using the native `pkgbuild` tool. Installers are placed in `dist/macos/`.

## Required tools

- `pkgbuild` — part of Xcode Command Line Tools on macOS. Install with `xcode-select --install`.

## Platform

PKG only processes binary artifacts targeting Darwin (macOS). Binaries for other operating systems are ignored.

## Minimal config

```yaml
crates:
  - name: myapp
    pkgs:
      - identifier: com.example.myapp
```

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `identifier` | string | **required** | Bundle identifier in reverse-domain notation (e.g. `com.example.myapp`). |
| `name` | string | `{{ ProjectName }}_{{ Version }}_{{ Arch }}.pkg` | Output filename template. |
| `install_location` | string | `/usr/local/bin` | Installation path on the target system. |
| `scripts` | string | | Path to a directory containing `preinstall` and/or `postinstall` scripts. |
| `extra_files` | list | | Additional files to include in the package payload. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the PKG. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds (e.g. `{{ .CommitTimestamp }}`). |
| `disable` | bool | `false` | Skip this PKG config. |

## How it works

For each macOS binary artifact, the stage:

1. Creates a temporary staging directory and copies the binary into it.
2. Copies any `extra_files` into the staging directory.
3. Applies `mod_timestamp` to all staged files if set.
4. Runs `pkgbuild --root <staging> --identifier <id> --version <ver> --install-location <path> <output.pkg>`.

If `scripts` is set, a `--scripts <dir>` argument is added pointing to your pre/postinstall scripts.

## Template variables

The `name` field supports standard template variables: `{{ ProjectName }}`, `{{ Version }}`, `{{ Arch }}`, `{{ Os }}`, `{{ Tag }}`.

## Install scripts

Place a `preinstall` and/or `postinstall` shell script in the directory specified by `scripts`. Both scripts must be executable. They are run by macOS Installer before and after the package payload is installed, respectively.

```
scripts/
  preinstall
  postinstall
```

## Full example

```yaml
crates:
  - name: myapp
    pkgs:
      - identifier: com.example.myapp
        name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}.pkg"
        install_location: /usr/local/bin
        scripts: installer/scripts
        extra_files:
          - LICENSE
        replace: true
        mod_timestamp: "{{ .CommitTimestamp }}"
```

## Signing

`pkgbuild` itself does not sign packages. To notarize for distribution outside the Mac App Store, pipe the output through `productsign` and then `xcrun notarytool` as a post-processing step in CI.
