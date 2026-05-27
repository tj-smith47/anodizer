+++
title = "macOS PKG"
description = "Build macOS installer packages using pkgbuild"
weight = 63
template = "docs.html"
+++

The PKG stage builds macOS `.pkg` installer packages from your Darwin binaries using the native `pkgbuild` tool. Installers are placed in `dist/macos/`.

## Classification

Packager — creates macOS PKG installers from Darwin binaries. Required: not a publisher; macOS only.

## Minimal config

```yaml
crates:
  - name: myapp
    pkgs:
      - identifier: com.example.myapp
```

## Full config reference

```yaml
crates:
  - name: myapp
    pkgs:
      - identifier: com.example.myapp  # required; reverse-domain bundle identifier
        id: ""                          # optional; unique identifier
        ids: []                         # optional; filter by build IDs
        name: ""                        # optional; output filename template (user controls extension)
        install_location: /usr/local/bin  # optional; installation path on target system
        scripts: ""                     # optional; directory with preinstall/postinstall scripts
        extra_files: []                 # optional; additional files in the package payload (anodizer-additive)
        templated_extra_files: []       # optional; extra files rendered through template engine (anodizer-additive)
        replace: false                  # optional; remove archive artifacts, keep PKG only
        mod_timestamp: ""               # optional; fixed timestamp for reproducible builds (templates allowed)
        skip: false                     # optional; also accepts `disable:` as an alias
```

## Authentication

Not applicable — PKG creation is a local build step using the native `pkgbuild` tool. Distribution/notarization requires a separate signing step (see [Signing](#signing)).

## Common gotchas

- **macOS only**: `pkgbuild` is not available on Linux. Cross-compilation is not supported.
- **Signing**: `pkgbuild` does not sign packages. For distribution outside the Mac App Store, pipe the output through the [notarize stage](../notarize/) using `use: pkg`. The notarize stage handles `productsign` and `xcrun notarytool` automatically.
- **`install_location`**: the default `/usr/local/bin` requires admin privileges. Use `/usr/local/bin` for CLI tools; use a user-writable path only for user-space tools.
- **Multi-arch identifier collisions**: pkg produces one package per binary. If `identifier:` is a literal string (no template vars), two builds for different architectures will share the same identifier. Disambiguate with a template (e.g. `com.example.myapp.{{ Arch }}`).

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Required tools

- `pkgbuild` — part of Xcode Command Line Tools on macOS. Install with `xcode-select --install`.

## Platform

PKG only processes binary artifacts targeting Darwin (macOS). Binaries for other operating systems are ignored.

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `identifier` | string | **required** | Bundle identifier in reverse-domain notation (e.g. `com.example.myapp`). Templates allowed. |
| `name` | string | `{{ ProjectName }}_{{ Arch }}` | Output filename template. User controls the extension — include `.pkg` explicitly if desired. |
| `install_location` | string | `/usr/local/bin` | Installation path on the target system. Templates allowed. |
| `scripts` | string | | Path to a directory containing `preinstall` and/or `postinstall` scripts. Templates allowed. |
| `extra_files` | list | | Additional files to include in the package payload. Anodizer-additive (not in GoReleaser Pro pkg). |
| `templated_extra_files` | list | | Extra files rendered through the template engine before inclusion. Anodizer-additive. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the PKG. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds. Templates allowed (e.g. `{{ CommitTimestamp }}`). Applied to the staging directory contents before `pkgbuild` bundles them — timestamps propagate into the pkg payload tar. |
| `skip` | bool/string | `false` | Skip this PKG config. Accepts `true`/`false` or a Tera template. Also accepts the `disable:` spelling for back-compat with imported GoReleaser configs. |
| `if` | string | | Template-conditional: skip if the rendered result is `false` or empty. Render failure is a hard error. |

## How it works

One `.pkg` is produced per binary — pkg installers are single-binary by design. Unlike DMG (which groups multiple binaries into one container image), each pkg wraps exactly one payload binary so that Homebrew formula installers and macOS Installer.app each target a discrete, independently versionable package. Multi-binary crates therefore emit N packages per target triple.

For each macOS binary artifact, the stage:

1. Creates a temporary staging directory and copies the binary into it.
2. Copies any `extra_files` into the staging directory (as-is).
3. Renders and copies any `templated_extra_files` into the staging directory.
4. Applies `mod_timestamp` to all staged files if set (template-rendered first).
5. Runs `pkgbuild --root <staging> --identifier <id> --version <ver> --install-location <path> <output>`.

If `scripts` is set, a `--scripts <dir>` argument is added pointing to your pre/postinstall scripts.

## Template variables

All of the following fields support standard template variables: `identifier`, `install_location`, `scripts`, `mod_timestamp`, and `name`.

Available variables: `{{ ProjectName }}`, `{{ Version }}`, `{{ Arch }}`, `{{ Os }}`, `{{ Tag }}`, `{{ CommitTimestamp }}`, and all other standard variables.

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
      - identifier: "com.example.{{ ProjectName }}"
        name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}.pkg"
        install_location: /usr/local/bin
        scripts: installer/scripts
        extra_files:
          - LICENSE
        replace: true
        mod_timestamp: "{{ CommitTimestamp }}"
```

## Signing

`pkgbuild` itself does not sign packages. To notarize for distribution outside the Mac App Store, pipe the output through the [notarize stage](../notarize/) with `use: pkg`. The notarize stage handles `productsign` (signing with a Developer ID Installer certificate) and `xcrun notarytool` submission automatically.
