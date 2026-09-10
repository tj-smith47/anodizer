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

Six template variables carry engine-generated POSIX-`sh` snippets for a
`curl | sh` installer script, derived from the release's configured targets and
the archive stage's own asset naming — so the script never hardcodes an asset
name that 404s, and the detection arms track the same vocabulary that keys the
asset arms instead of a hand-written `uname` mapping that silently drifts:

| Variable | Contents |
|----------|----------|
| `InstallerAssetCases` | Asset arms mapping each released platform to its exact asset filename (sets `ARCHIVE=` and `FORMAT=`) |
| `InstallerAssetCaseSubject` | The word the asset `case` matches on — `${OS}-${ARCH}`, or `${OS}-${ARCH}-${LIBC}` once a platform ships two libcs |
| `InstallerDetectOsCases` | `case "$(uname -s)"` arms echoing the OS tokens the asset arms are keyed by |
| `InstallerDetectArchCases` | `case "$(uname -m)"` arms echoing the arch tokens the asset arms are keyed by |
| `InstallerDetectLibc` | A block setting `LIBC` to `gnu` or `musl`, emitted only when some platform ships both. Empty otherwise |
| `InstallerSupportedPlatforms` | The reachable platform keys, space-joined — for error messages that list what IS available |

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
{{ InstallerDetectLibc }}
case "{{ InstallerAssetCaseSubject }}" in
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
precedence.

## glibc and musl on the same architecture

A statically-linked musl binary and a glibc binary of the same architecture
both reduce to `linux-amd64`, but they are not interchangeable: a glibc binary
cannot run on Alpine. When a release ships both, the asset arms split by libc
and `InstallerDetectLibc` renders the probe that chooses between them:

```sh
LIBC=gnu
if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
	LIBC=musl
elif ls /lib/ld-musl-* >/dev/null 2>&1 && ! ls /lib/ld-linux-*.so.* >/dev/null 2>&1; then
	LIBC=musl
fi
```

`InstallerAssetCaseSubject` then renders `${OS}-${ARCH}-${LIBC}`, the split
platform gets one arm per libc, and every other platform is emitted with a
trailing glob so it still matches:

```sh
    darwin-arm64-*)
        ARCHIVE="myapp_${version}_aarch64-apple-darwin.tar.gz"
        FORMAT="tar.gz"
        ;;
    linux-amd64-gnu)
        ARCHIVE="myapp_${version}_x86_64-unknown-linux-gnu.tar.gz"
        FORMAT="tar.gz"
        ;;
    linux-amd64-musl)
        ARCHIVE="myapp_${version}_x86_64-unknown-linux-musl.tar.gz"
        FORMAT="tar.gz"
        ;;
```

A release with one libc per platform pays nothing for this:
`InstallerDetectLibc` renders empty, `InstallerAssetCaseSubject` renders
`${OS}-${ARCH}`, and the arms keep their plain keys. A musl-only release does
not split either — a static musl binary runs on glibc hosts too, and neither
does a release whose two libc builds are archived under one asset name: two
arms naming one file would advertise a platform the release never uploads.

**Match the case on `InstallerAssetCaseSubject`, never on a hand-written
`"${OS}-${ARCH}"`.** A template that reads `InstallerAssetCases` but keeps its
own subject matches nothing the day its project starts shipping both libcs —
every host would fall through to the unsupported-platform error. anodizer
refuses that render instead, failing the templatefiles stage with:

```text
template_files id 'install' (src 'scripts/install.sh.tpl') builds an installer from the engine case arms but never reads InstallerAssetCaseSubject. This release ships both glibc and musl builds for at least one platform, so the arms are keyed '${OS}-${ARCH}-${LIBC}' and a script matching on '${OS}-${ARCH}' matches none of them. Emit InstallerDetectLibc after the arch detection and match the case on InstallerAssetCaseSubject
```

The mips family is deliberately absent from the generated `uname -m` arms:
`uname -m` reports `mips`/`mips64` for both endiannesses, so the script cannot
safely choose between same-name big- and little-endian assets — mips hosts get
the explicit unsupported-platform error rather than a wrong-endian binary.
illumos hosts (`uname -s` = `SunOS`, mapped to `solaris`) are likewise
undetectable. Releasing such a target still emits its asset arm, but anodizer
prints a warning naming the stranded target so you know those hosts fall
through to the error path.

Each snippet omits the `*)` fallback arm — your template owns the error path.
`InstallerSupportedPlatforms` is made for exactly that arm: it lists the keys
a host can actually reach, so the error can point users at the assets that do
exist:

```sh
    *)
        echo "Error: no prebuilt ${PROJECT} binary for ${OS}/${ARCH}" >&2
        echo "Prebuilt binaries: {{ InstallerSupportedPlatforms }}" >&2
        exit 1
        ;;
```

renders as:

```sh
        echo "Prebuilt binaries: darwin-amd64 darwin-arm64 linux-amd64 linux-arm64 windows-amd64 windows-arm64" >&2
```

Every variable renders empty when no crate builds a binary named after the
project with a binstallable archive — except `InstallerAssetCaseSubject`,
which always names a usable `case` subject.
