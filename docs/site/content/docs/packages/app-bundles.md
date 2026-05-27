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
        min_os_version: "10.13"       # optional; LSMinimumSystemVersion in Info.plist
        extra_files: []               # optional; additional files to include in Resources
        templated_extra_files: []     # optional; template-rendered extra files
        mod_timestamp: ""             # optional; override mtime for reproducible builds
        replace: false                # optional; remove source archives, keep app bundle
        disable: false                # optional; bool or template string
```

## Authentication

Not applicable — app bundle creation is a local build step with no external service calls.

## Common gotchas

- **macOS only**: the stage only processes Darwin binary artifacts. Non-macOS targets are ignored.
- **`.app` extension**: auto-appended if not present in `name`. The directory must end in `.app` for macOS to treat it as an application bundle, so unlike `.pkg` / `.msi` / `.nsis` / `.dmg` (where the user controls the extension) anodizer always ensures the suffix.
- **`icon` must be `.icns`**: other formats (`.png`, `.svg`) are not accepted by macOS for the `CFBundleIconFile` key.
- **`bundle:` has no default**: a missing `bundle:` is a hard error. Anodizer does not invent an identifier under `com.anodizer.*` because that namespace is not owned by you — using it would conflict with App Store submission and notarization.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Signing and Notarization

An `.app` bundle produced by this stage is **unsigned and un-notarized**.
macOS Gatekeeper will reject it with the "cannot be opened because the
developer cannot be verified" dialog unless you also run the
`notarize.macos_native` stage downstream.

Wire it up like this:

```yaml
notarize:
  macos_native:
    sign:
      certificate_name: "Developer ID Application: My Org (TEAMID)"
      entitlements: "macos/entitlements.plist"
      options: ["runtime"]   # enables hardened runtime
    notarize:
      apple_id: "you@example.com"
      team_id: "TEAMID"
      key: "{{ .Env.AC_API_KEY }}"
      key_id: "{{ .Env.AC_API_KEY_ID }}"
      issuer: "{{ .Env.AC_API_ISSUER }}"
```

`entitlements` and `options: [runtime]` (hardened runtime) are not knobs on
`app_bundles:` itself — they live on the notarize stage, which signs the
generated `.app` and then notarizes it through Apple. See the
[notarize docs](../general/sign/) for the full set of options.

## App bundle config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier for this config |
| `ids` | list | all builds | Filter by build IDs |
| `name` | string | `{{ ProjectName }}_{{ Arch }}.app` | Output .app bundle name (template) |
| `icon` | string | none | Path to `.icns` icon file (template) |
| `bundle` | string | **required** | Bundle identifier in reverse-DNS notation |
| `min_os_version` | string | `10.13` | Written to `LSMinimumSystemVersion` in `Info.plist` |
| `extra_files` | list | none | Additional files to include in Resources |
| `templated_extra_files` | list | none | Template-rendered extra files |
| `mod_timestamp` | string | none | Override mtime for reproducible builds (template) |
| `replace` | bool | `false` | Remove source archives, keeping only the app bundle (anodizer-additive — not in GoReleaser Pro's `app_bundles:`) |
| `disable` | string/bool | none | Disable this config (bool or template) |

### `extra_files` shape

Each entry is either a glob string or a detailed object:

```yaml
extra_files:
  - LICENSE                          # glob, copies into Contents/Resources/
  - src: "docs/*.txt"                # glob with explicit destination
    dst: "Contents/SharedSupport"
  - src: "assets/manual.pdf"         # per-file mtime for reproducibility
    info:
      mtime: "{{ .CommitDate }}"
```

The `info` block supports `mtime` (RFC3339 or `SOURCE_DATE_EPOCH` integer
seconds). When both `extra_files[].info.mtime` and the bundle-level
`mod_timestamp` are set, the per-file value wins because it is the more
specific knob.

The `info.mode`, `info.owner`, and `info.group` fields are accepted by the
schema but **not used for app bundles** (macOS reads file metadata from the
filesystem, not from a manifest). Anodizer emits a warning when any of them
are set so the no-op is visible.

**Glob + `dst` rename caveat**: when `dst` is set, the `src` glob must match
exactly one file. A multi-match glob paired with a constant `dst` would
silently overwrite every earlier copy with the last one — anodizer fails
pre-flight instead. Use a directory `dst` (no rename) or narrow the glob.

### Overriding the generated `Info.plist`

The auto-generated `Info.plist` is written first, then `extra_files` are copied
in. A user-supplied entry whose `dst` resolves to `Contents/Info.plist`
therefore overwrites the generated file:

```yaml
crates:
  - name: myapp
    app_bundles:
      - bundle: com.example.myapp
        extra_files:
          - src: macos/Info.plist
            dst: Contents/Info.plist
```

Use this when you need keys anodizer does not emit (`NSCameraUsageDescription`,
`CFBundleURLTypes`, `UIBackgroundModes`, custom document types, etc.).

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
| `LSMinimumSystemVersion` | `min_os_version` (default `10.13`) |
| `CFBundleIconFile` | Icon filename (when `icon` is set) |

If you need a key not in this list, supply your own `Info.plist` via the
[override pattern](#overriding-the-generated-infoplist).

## Defaults vs GoReleaser

| Knob | GoReleaser Pro | Anodizer |
|------|----------------|----------|
| `name` default | `{{ .ProjectName }}` | `{{ ProjectName }}_{{ Arch }}.app` |
| `bundle` default | none (user-supplied) | none (hard error if unset) |
| `replace` on `app_bundles:` | not exposed | exposed (anodizer-additive) |
| `min_os_version` | not exposed | exposed (anodizer-additive) |

The `_{{ Arch }}` suffix on the default `name` is deliberate: a multi-arch
build (`arm64` + `amd64`) under GoReleaser's bare `{{ .ProjectName }}` would
produce two `MyApp.app` directories whose second write overwrites the first.
Anodizer's default keeps both artifacts addressable. If you are importing a
GoReleaser config and want the original behavior, set `name` explicitly.

## Behavior

- Only processes macOS (darwin) binary artifacts
- The `.app` extension is auto-appended if not present in `name`
- Output is placed in `dist/macos/`
- `mod_timestamp` is applied recursively to the entire `.app` tree
- Per-file `extra_files[].info.mtime` overrides the bundle-level
  `mod_timestamp` for the file it points at
- Skippable with `--skip appbundle`

## Full example

```yaml
crates:
  - name: myapp
    app_bundles:
      - name: "MyApp_{{ Arch }}.app"
        bundle: com.myorg.myapp
        icon: assets/icon.icns
        min_os_version: "12.0"
        extra_files:
          - LICENSE
          - README.md
          - src: macos/Info.plist
            dst: Contents/Info.plist
          - src: assets/manual.pdf
            info:
              mtime: "{{ .CommitDate }}"
        mod_timestamp: "{{ .CommitTimestamp }}"
        replace: true
```
