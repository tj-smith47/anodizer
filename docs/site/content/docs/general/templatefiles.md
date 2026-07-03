+++
title = "Template Files"
description = "Render template files through the template engine and include them in releases"
weight = 10
template = "docs.html"
+++

The `template_files` stage renders source files through the template engine and automatically uploads the output as release assets.

## Minimal config

```yaml
template_files:
  - src: install.sh.tpl
    dst: install.sh
```

## Template files config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | `default` | Identifier for this template file entry |
| `src` | string | — | Source template file path (supports templates) |
| `dst` | string | — | Destination filename within the dist directory (supports templates) |
| `mode` | string | `0655` | File permissions in octal notation (Unix only) |

## Behavior

- Both `src` and `dst` paths are rendered through the template engine before use
- The source file contents are also rendered through the template engine
- Output files are written to `dist/<dst>`
- Each output is registered as an uploadable artifact (included in releases, checksums, and signing)
- Path traversal (`..`) and absolute paths in `dst` are rejected for security
- The stage can be skipped with `--skip templatefiles`

## Template rendering

Use any template variable in both the file paths and file contents:

```yaml
template_files:
  - id: install-script
    src: "scripts/{{ ProjectName }}-install.sh.tpl"
    dst: "{{ ProjectName }}-{{ Version }}-install.sh"
    mode: "0755"
```

Given a source file `scripts/myapp-install.sh.tpl`:

```bash
#!/bin/sh
# Install {{ ProjectName }} {{ Version }}
curl -L https://github.com/myorg/{{ ProjectName }}/releases/download/{{ Tag }}/{{ ProjectName }}-{{ Os }}-{{ Arch }}.tar.gz | tar xz
```

## Setting file permissions

Use the `mode` field to set executable permissions on generated scripts:

```yaml
template_files:
  - src: run.sh.tpl
    dst: run.sh
    mode: "0755"
```

The mode must be a string in octal notation (e.g., `"0755"`, `"0644"`). The default is `0655`.

## Multiple template files

```yaml
template_files:
  - id: install-script
    src: install.sh.tpl
    dst: install.sh
    mode: "0755"
  - id: config-example
    src: config.yaml.tpl
    dst: config.example.yaml
  - id: completion
    src: completions/bash.tpl
    dst: "{{ ProjectName }}.bash"
```

Each entry gets its own artifact ID, so you can reference them individually in publisher configs.

## Remote installer case tables

Three template variables carry engine-generated POSIX-`sh` `case` arms for a
`curl | sh` installer script, derived from the release's configured targets and
the archive stage's own asset naming — so the script never hardcodes an asset
name that 404s or a `uname` mapping that strands a released target:

| Variable | Contents |
|----------|----------|
| `InstallerAssetCases` | `case "${OS}-${ARCH}"` arms mapping each released `os-arch` pair to its exact asset filename (sets `ARCHIVE=`) |
| `InstallerDetectOsCases` | `case "$(uname -s)"` arms echoing the OS tokens the asset arms are keyed by |
| `InstallerDetectArchCases` | `case "$(uname -m)"` arms echoing the arch tokens the asset arms are keyed by |

```bash
#!/bin/sh
detect_os() {
    case "$(uname -s)" in
{{ InstallerDetectOsCases }}
        *) echo "unsupported" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
{{ InstallerDetectArchCases }}
        *) echo "unsupported" ;;
    esac
}

OS="$(detect_os)"; ARCH="$(detect_arch)"
case "${OS}-${ARCH}" in
{{ InstallerAssetCases }}
    *) echo "no prebuilt binary for ${OS}/${ARCH}" >&2; exit 1 ;;
esac
curl -sSfL "https://github.com/me/{{ ProjectName }}/releases/download/{{ Tag }}/${ARCHIVE}"
```

Rendered for a release targeting Linux/macOS/Windows on amd64+arm64, the
detection arms come out as:

```sh
        Linux*) echo "linux" ;;
        Darwin*) echo "darwin" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
```

and each asset arm resolves to the same filename the archive stage uploads
(`ARCHIVE="myapp_1.2.3_linux_amd64.tar.gz"`), including `format_overrides`
(e.g. `zip` on Windows). A `darwin-universal` build is fanned out to the
`darwin-amd64` / `darwin-arm64` keys, with arch-specific assets taking
precedence. Each snippet omits the `*)` fallback arm — your template owns the
error path. All three render empty when no crate builds a binary named after
the project with a binstallable archive.
