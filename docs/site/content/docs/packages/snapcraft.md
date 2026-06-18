+++
title = "Snapcraft"
description = "Build and publish Snap packages for the Snapcraft store"
weight = 60
template = "docs.html"
+++

The snapcraft stage builds `.snap` packages from your Linux binaries and optionally publishes them to the [Snap Store](https://snapcraft.io/).

## Classification

Packager + optional Submitter — builds snap packages from Linux binaries and, when `publish: true` is set, uploads them to the Snap Store. The Snap Store upload is a Submitter-group operation (no rollback; already-installed snaps keep the revision).

## Minimal config

```yaml
crates:
  - name: myapp
    snapcrafts:
      - summary: "My application"
        description: "A longer description shown in the Snap Store."
```

## Full config reference

```yaml
crates:
  - name: myapp
    snapcrafts:
      - id: ""                         # optional; unique identifier
        ids: []                        # optional; filter by build IDs
        name: ""                       # optional; snap package name (default: binary name)
        title: ""                      # optional; user-facing application title
        summary: ""                    # optional; single-line description (max 78 chars)
        description: ""                # optional; extended description
        icon: ""                       # optional; path to .png or .svg icon
        base: core22                   # optional; core | core18 | core20 | core22 | core24 | bare
        grade: ""                      # optional; stable | devel
        license: ""                    # optional; SPDX identifier
        confinement: strict            # optional; strict | devmode | classic
        plugs: {}                      # optional; interface plug definitions
        assumes: []                    # optional; required snapd features
        apps: {}                       # optional; named app entries (auto-generated if omitted)
        layout: {}                     # optional; filesystem layout mappings (accepts `layouts:` alias)
        extra_files: []                # optional; additional static files to bundle
        name_template: ""              # optional; output filename template
        publish: false                 # optional; upload to Snap Store after building
        channel_templates: []          # optional; store channels to release to
        replace: false                 # optional; remove archive artifacts, keep snap only
        mod_timestamp: ""             # optional; fixed timestamp for reproducible builds
        skip: false                    # optional; accepts `disable:` alias (deprecation-warned)
```

## Authentication

| Variable | Description |
|----------|-------------|
| `SNAPCRAFT_STORE_CREDENTIALS` | Snapcraft login credentials (base64-encoded). Obtain via `snapcraft export-login --snaps myapp --channels stable -`. |

Alternatively, run `snapcraft login` before releasing to authenticate interactively.

## Common gotchas

- **`snapcraft` must be on `PATH`**: install via `sudo snap install snapcraft --classic`.
- **Linux only**: the stage ignores non-Linux artifacts. Ensure at least one Linux build target is configured.
- **Icon auto-write**: the `icon` field copies the file to `meta/gui/<name>.<ext>` inside the staged prime directory before `snapcraft pack` runs. The source path may be absolute or relative to the project root. The icon does NOT appear in `snap.json`.
- **`grade: stable` + `confinement: devmode`**: snapcraft will warn that `devmode` snaps cannot be published to the `stable` channel.

## Republish / update behavior

Not applicable — once a snap revision is uploaded to the Snap Store, it cannot be removed. Already-installed snaps keep the revision they installed. Use a different `channel_templates` target (e.g., `edge`) for pre-release builds.

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `name` | string | binary name | Snap package name in the store. |
| `title` | string | | User-facing application title. |
| `summary` | string | crate `description` | Single-line description (max 78 characters). Falls back to the resolved `description` (which itself falls back to the crate's `Cargo.toml` `package.description`); the result is hard-capped at 78 characters. |
| `description` | string | | Extended description shown in the store. |
| `icon` | string | | Path to the snap icon image (`.png` or `.svg`). Anodizer copies the file to `meta/gui/<name>.<ext>` inside the staged prime directory before `snapcraft pack` runs. The icon is picked up by snapcraft via the GUI metadata channel and does NOT appear in `snap.json`, keeping uploads schema-clean. The source path may be absolute or relative to the project root. |
| `base` | string | `core22` | Base snap: `core`, `core18`, `core20`, `core22`, `core24`, `bare`. |
| `grade` | string | | Release quality: `stable` or `devel`. |
| `license` | string | | SPDX license identifier. |
| `confinement` | string | `strict` | Security model: `strict`, `devmode`, or `classic`. |
| `plugs` | map | | Interface plug definitions (HashMap\<String, Value\>). Keys are plug names; values are plug attributes. |
| `assumes` | list | | Required snapd features or minimum versions. |
| `apps` | map | auto | Named app entries. Auto-generates a default entry from the first binary if omitted. |
| `layout` | map | | Filesystem layout mappings for sandbox accessibility. The plural `layouts:` spelling is still accepted via serde alias. |
| `extra_files` | list | | Additional static files to bundle in the snap. |
| `name_template` | string | `<name>_<version>_<arch>.snap` | Output filename template. |
| `publish` | bool | `false` | Upload to the Snap Store after building. |
| `channel_templates` | list | | Store channels to release to (e.g. `edge`, `beta`, `stable`). |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the snap. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds (e.g. `{{ CommitTimestamp }}`). |
| `skip` | bool/template | `false` | Skip this snapcraft config. The GoReleaser `disable:` spelling is accepted as a deprecation-warned alias. |

### Confinement values

| Value | Description |
|-------|-------------|
| `strict` | Fully confined to declared interfaces (default, recommended for production). |
| `devmode` | Development mode — no confinement enforcement, useful for testing. |
| `classic` | Traditional package — no sandbox, requires Snap Store approval. |

### App config (`apps`)

Each entry under `apps` describes an application exposed by the snap.

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Command path relative to the snap root. Defaults to the app's name when omitted. |
| `args` | string | Additional arguments appended to the command. |
| `daemon` | string | Run as a daemon: `simple`, `forking`, `oneshot`, `notify`, `dbus`. |
| `stop_mode` | string | Signal used to stop the daemon: `sigterm`, `sigkill`, etc. |
| `restart_condition` | string | When to restart: `on-failure`, `always`, `never`, etc. |
| `plugs` | list | Interfaces this app needs. |
| `slots` | list | Interface slots this app provides (this is the only place snap slots are configured; there is no top-level `slots`). |
| `environment` | map | Environment variables for the app. |

When no `apps` map is provided, a default entry is generated using the first binary's name with the command `bin/<name>`.

### Layout config (`layout`)

| Field | Type | Description |
|-------|------|-------------|
| `bind` | string | Bind-mount a host directory into the snap. |
| `symlink` | string | Create a symlink within the snap's layout. |

## Architecture mapping

Target triple components are mapped to Snapcraft architecture names:

| Rust target | Snap arch |
|-------------|-----------|
| `x86_64` / `amd64` | `amd64` |
| `aarch64` / `arm64` | `arm64` |
| `armv7` | `armhf` |
| `i686` / `i386` | `i386` |
| `s390x` | `s390x` |
| `ppc64le` | `ppc64el` |
| `riscv64` | `riscv64` |

## Publishing to the Snap Store

Set `publish: true` and authenticate with `snapcraft login` (or set `SNAPCRAFT_STORE_CREDENTIALS`) before running anodizer. When `channel_templates` is provided, the snap is released to those channels automatically via `snapcraft upload --release`.

## Full example

```yaml
crates:
  - name: myapp
    snapcrafts:
      - name: myapp
        title: "My Application"
        summary: "A fast, cross-platform tool"
        description: |
          A longer description displayed in the Snap Store.
          Supports multiple paragraphs.
        base: core22
        grade: stable
        confinement: strict
        license: MIT
        plugs:
          home: {}
          network: {}
        apps:
          myapp:
            command: bin/myapp
            plugs:
              - home
              - network
        extra_files:
          - LICENSE
        publish: true
        channel_templates:
          - edge
          - stable
        replace: true
```
