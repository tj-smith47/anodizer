+++
title = "Install script"
description = "Emit a curl | sh POSIX installer as a release asset"
weight = 66
template = "docs.html"
+++

Anodizer emits a deterministic POSIX `install.sh` (`curl | sh`) installer as a
release asset. At run time the script detects the host OS + architecture, maps
it to the matching release archive, downloads and (by default) sha256-verifies
it, extracts the binary, and installs it — defaulting to the latest GitHub
release, overridable with a `VERSION=` environment variable.

This packager is Rust-additive: it has no GoReleaser equivalent.

There are two ways to ship a `curl | sh` installer:

- **This `install_scripts:` block** — the batteries-included path. Anodizer owns
  a proven download → verify → extract → install skeleton and you supply only
  metadata. Start here.
- **A hand-written [`template_files:`](../../general/templatefiles/) entry** —
  the advanced path. You own the whole script and consume the same engine-derived
  case tables (`InstallerAssetCases`, `InstallerAssetCaseSubject`,
  `InstallerDetectOsCases`, `InstallerDetectArchCases`, `InstallerDetectLibc`,
  `InstallerSupportedPlatforms`) as template variables. Reach for this when you need a bespoke installer shape.

Both paths derive their per-platform asset table from the same engine SSOT
(`render_installer_cases`), so neither can bake an asset name that 404s.

## Classification

Packager — writes a text `install.sh` asset. Its case tables come from the
release's configured targets (via the archive stage's own asset-naming SSOT),
not from produced artifacts, so the output is byte-identical on every build.
Not a publisher; spawns no external tool at build time.

## Minimal config

```yaml
install_scripts: {}
```

That bare form works with no required input: the repository slug is derived from
the git `origin` remote, the installed binary name from the project name, the
per-platform asset names from the configured targets, and the checksums filename
and tag prefix from the flagship crate that builds the project binary.

## Full config reference

```yaml
install_scripts:
  - id: default                  # optional; unique identifier
    filename: install.sh         # optional; output filename
    binaries: [myapp]            # optional; binaries to install (default: [project name])
    repo: acme/myapp             # optional; owner/name (default: from git origin remote)
    base_url: https://github.com # optional; download + API base (GHE-aware)
    verify_checksum: true        # optional; gate the sha256 verification step
    install_dir: /usr/local/bin  # optional; install directory
    name: myapp                  # optional; header-banner name (default: project name)
    description: "My tool"        # optional; header-banner description
    homepage: https://myapp.dev  # optional; header-banner homepage
    skip: false                  # optional; bool or template string
```

## Derived defaults

Everything the tool can compute is optional with a correct derived default —
you require none of it:

| Field | Derived default |
|-------|-----------------|
| `filename` | `install.sh` |
| `binaries` | a single-element list of the project name |
| `repo` | `owner/name` parsed from the git `origin` remote |
| `base_url` | `https://github.com` (API base derived as `<base_url>/api/v3` for GHE) |
| `verify_checksum` | `true` |
| `install_dir` | `/usr/local/bin` (falls back to `$HOME/.local/bin` at run time) |
| asset names | derived from the configured targets, version-templated |
| checksums filename | the flagship crate's checksum `name_template`, version-templated |
| tag prefix | the flagship crate's `tag_template` (`v{{ Version }}` → `v`) |

## Usage

Install the latest release:

```bash
curl -fsSL https://github.com/acme/myapp/releases/download/v1.2.3/install.sh | sh
```

Pin a specific release with `VERSION=` (accepts `1.2.3` or the full tag such as
`v1.2.3`):

```bash
curl -fsSL https://github.com/acme/myapp/releases/latest/download/install.sh | VERSION=v1.2.0 sh
```

Override the install directory with `INSTALL_DIR=`:

```bash
curl -fsSL .../install.sh | INSTALL_DIR="$HOME/bin" sh
```

`--help` prints that surface from the script itself, so a `curl | sh` user who
never opens the file can still discover it:

```console
$ sh install.sh --help
myapp installer — installs myapp from acme/myapp releases.

Usage:
  sh install.sh [--help]
  curl -fsSL https://github.com/acme/myapp/releases/latest/download/install.sh | sh

Environment:
  VERSION       release to install (default: the latest release);
                accepts a bare version or the full tag, e.g. v1.2.3
  INSTALL_DIR   directory to install into (default: /usr/local/bin);
                falls back to $HOME/.local/bin when it is not writable

Supported platforms: darwin-amd64 darwin-arm64 linux-amd64 linux-arm64
```

`--help` is the only flag; every other argument is a typo and is refused rather
than ignored:

```console
$ sh install.sh --bogus
myapp-install: error: unknown argument: --bogus (try --help)
```

## What the generated script does

1. Detects `uname -s` / `uname -m` and maps the pair to the matching release
   archive via engine-generated `case` arms. The detection vocabulary is the
   same `map_target` OS/arch tokens that key the asset arms, so a released
   target can never be stranded behind a hand-written mapping. Linux, macOS, and
   Windows (MSYS/Cygwin/MinGW) hosts are all served when the release targets
   them. When a release ships BOTH a glibc and a musl build of one
   architecture, the script also probes the host's libc and the arms are keyed
   `linux-amd64-gnu` / `linux-amd64-musl` — see below.
2. Resolves the version — from `VERSION=` when set, otherwise the latest
   release's `tag_name` from the REST API (no `jq` dependency). The configured
   tag prefix is stripped to get the bare version and re-applied to build the
   tag, so a non-`v` prefix (e.g. `release-`) works.
3. Downloads the archive and, when `verify_checksum` is true, the combined
   checksums file (or an `<asset>.sha256` sidecar) into a `mktemp -d` directory,
   then verifies the sha256 with `sha256sum` or `shasum -a 256`, aborting on
   mismatch.
4. Extracts the asset. Every format the archive stage can assign is handled,
   dispatched on the format the engine baked into the matching arm rather than
   re-derived from the filename:

   | Format | Extracted with |
   |---|---|
   | `tar.gz`, `tgz` | `tar -xzf` |
   | `tar.xz`, `txz` | `tar -xJf` (needs `xz`) |
   | `tar.zst`, `tzst` | `tar --use-compress-program=zstd -xf` (needs `zstd`) |
   | `tar` | `tar -xf` |
   | `zip` | `unzip -q` (needs `unzip`) |
   | `gz` | `gunzip -c` onto the binary name |
   | `xz` | `xz -dc` onto the binary name |
   | `binary` | copied as-is — the asset *is* the executable |

   An unknown format aborts with `unknown archive format: <format> (<asset>)`,
   and a missing decompressor aborts with `need <tool> to extract <asset>`
   instead of leaving a half-installed directory.

5. Installs every binary in `binaries` with `install -m 0755` into the install
   directory — trying `sudo` when the directory is not writable, then falling
   back to `$HOME/.local/bin`. Whichever directory the binary lands in, the
   script warns when it is not on `$PATH`.
6. Cleans up the temp directory on exit via a `trap`.

## glibc and musl

`x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` both reduce to
`linux-amd64`, but a glibc binary cannot run on Alpine. When a release ships
both, the script probes the host at install time and each libc keeps its own
arm:

```sh
LIBC=gnu
if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
	LIBC=musl
elif ls /lib/ld-musl-* >/dev/null 2>&1 && ! ls /lib/ld-linux-*.so.* >/dev/null 2>&1; then
	LIBC=musl
fi

case "${OS}-${ARCH}-${LIBC}" in
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
esac
```

`--help` and the unsupported-platform error list the split keys, so an
operator sees exactly which libcs are published:

```console
$ sh install.sh --help
...
Supported platforms: darwin-amd64 darwin-arm64 linux-amd64-gnu linux-amd64-musl
```

Nothing changes for a release that ships one libc per platform: no probe is
emitted, the `case` subject stays `${OS}-${ARCH}`, and the arms keep their
plain keys. A musl-only release does not split either — a static musl binary
runs on glibc hosts too. A host that reveals neither an `ldd` nor a loader
falls back to `gnu`.

## GitHub Enterprise

Point `base_url` at a self-hosted GitHub to install from it:

```yaml
install_scripts:
  base_url: https://github.example.com
```

The script downloads from `<base_url>/<repo>/releases/...` and queries the REST
API at `<base_url>/api/v3` for any non-`github.com` host.

## Common gotchas

- **POSIX `sh`, not bash**: the script targets `#!/bin/sh` with `set -eu` and
  contains no bashisms.
- **Deterministic**: no timestamps, no `$RANDOM`, no read of produced
  artifacts; the asset arms are engine-derived in a stable order, so the emitted
  script is byte-identical across runs and determinism shards.
- **Archive formats**: every format the archive stage assigns is extracted (the
  table in step 4), honoring `format_overrides` — e.g. `zip` on Windows. The
  format travels with the asset name in the generated arm, so a per-target
  override never strands a platform on the wrong extractor.
- **Single-file formats need a single binary**: `gz`, `xz` and `binary` assets
  hold one executable and carry no directory entries, so there is no name to
  extract by. Configuring one of them alongside two or more `binaries` is
  refused at render time rather than emitting an installer that would misname
  the result.
- **`curl` or `wget`**: the script uses whichever is present; one is required on
  the target host, along with `sha256sum`/`shasum` (unless
  `verify_checksum: false`) and whichever decompressor step 4 lists for the
  formats you ship.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher. Each release
emits a fresh `install.sh` alongside the archives.
