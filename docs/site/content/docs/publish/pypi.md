+++
title = "PyPI"
description = "Publish prebuilt binaries as native Python wheels on PyPI"
weight = 87
template = "docs.html"
+++

Anodizer publishes your compiled binaries as **native Python wheels**, letting users install your CLI via `pip install <name>` (or `pipx install <name>`). Each built target becomes one `py3-none-<platform>` wheel carrying the prebuilt executable under the wheel's `.data/scripts/` directory, so pip drops it straight onto the console-script `PATH` — the same layout maturin's `bindings = "bin"` mode emits, with no Python code and no compilation on the user's machine.

The platform tag is **derived by inspecting each binary**, never guessed:

| Built target | Inspection | Wheel platform tag |
|---|---|---|
| `x86_64-unknown-linux-gnu` | max `GLIBC_*` requirement in the ELF (e.g. 2.28) | `manylinux_2_28_x86_64` |
| `aarch64-unknown-linux-gnu` | max `GLIBC_*` requirement (e.g. 2.17) | `manylinux_2_17_aarch64` |
| `x86_64-unknown-linux-musl` | none needed (static musl) | `musllinux_1_2_x86_64` |
| `aarch64-unknown-linux-musl` | none needed | `musllinux_1_2_aarch64` |
| `x86_64-apple-darwin` | Mach-O deployment target (`LC_BUILD_VERSION`, e.g. 10.13) | `macosx_10_13_x86_64` |
| `aarch64-apple-darwin` | Mach-O deployment target (e.g. 11.0) | `macosx_11_0_arm64` |
| universal (fat) darwin binary | max deployment target across slices | `macosx_11_0_universal2` |
| `x86_64-pc-windows-msvc` | — | `win_amd64` |
| `i686-pc-windows-msvc` | — | `win32` |
| `aarch64-pc-windows-msvc` | — | `win_arm64` |

Because the `manylinux` tag comes from the binary's *real* glibc floor, a wheel never claims broader compatibility than the executable actually has. A gnu-target binary that declares **no** glibc requirement is a hard error — that means the wrong binary landed under that target. Likewise a darwin-target artifact that is **not** a Mach-O object is a hard error (the Mach-O analogue of the missing-glibc case). When a Mach-O carries no version load command, the tag falls back to `10_12` (x86_64) / `11_0` (arm64 and universal). macOS 11+ deployment targets always tag `macosx_<major>_0` (e.g. an 11.2 minos wheel tags `macosx_11_0`), matching what pip/packaging enumerate. A binary whose only glibc requirement is the ancient x86_64 baseline (`GLIBC_2.2.5`) floors to `manylinux_2_5` rather than the unrecognized `manylinux_2_2`.

### One binary per platform per entry

A wheel filename carries the **project name**, so two binaries that resolve to the same platform tag would collide on one identical `.whl` — the second silently overwriting the first (or, on the index, being rejected as a duplicate). This happens in a **multi-binary workspace** where more than one crate builds the same target triple. Give each `pypis[]` entry its own [`ids:`](#configuration) so it selects exactly one binary per platform:

```yaml
pypis:
  - name: tool-a
    ids: [crate-a]        # crate-a's binaries only
  - name: tool-b
    ids: [crate-b]        # crate-b's binaries only
```

`anodizer preflight` warns when the selected crates would build the same triple more than once, and the publish itself hard-errors on an actual duplicate platform tag.

### Publishing a subset of targets

`targets:` restricts this entry to a subset of the built target triples — only binaries whose triple is listed become wheels; the rest are silently skipped. It is orthogonal to `ids:` (both filters apply). Besides trimming what ships, it resolves a same-platform-tag collision without splitting into separate entries: `x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc` both tag `win_amd64`, so building both would collide on one `.whl` — list only the one you publish:

```yaml
pypis:
  - name: git-cliff
    targets:
      - x86_64-unknown-linux-gnu
      - aarch64-unknown-linux-gnu
      - x86_64-pc-windows-msvc      # gnu-windows omitted — no win_amd64 collision
      - x86_64-apple-darwin
      - aarch64-apple-darwin
```

The collision preflight honours the allowlist (a triple filtered out cannot collide), and a listed triple that no selected build produces is a config error naming the offending triple.

## Classification

| Group | Required (default) | Rollback | Token |
|-------|--------------------|----------|-------|
| Manager | `true` | **none — one-way door** | `PYPI_TOKEN` / `MATURIN_PYPI_TOKEN` |

**PyPI uploads are a one-way door.** A published filename can never be re-uploaded — even after deleting the file or the release — so there is no programmatic rollback (rollback is warn-only). Like cargo, a failed release is fixed *forward* to the next version. Re-runs of an already-published tag are safe: `skip_existing` (default `true`) folds the index's "file already exists" rejection into an idempotent skip.

## Quick start

```yaml
pypis:
  - requires_python: ">=3.7"
```

Run with `PYPI_TOKEN=pypi-...` exported. Everything else is derived: the project name falls back to the crate name, `summary`/`homepage`/`license` fall back to the project metadata (and the crate's `Cargo.toml`), the wheel version is the release version mapped to PEP 440, and each built binary contributes one wheel with an inspected platform tag.

```console
$ anodizer release
  • processing pypi project 'pypis[0]'
  • built wheel my_tool-1.2.3-py3-none-manylinux_2_28_x86_64.whl (manylinux_2_28_x86_64)
  • built wheel my_tool-1.2.3-py3-none-macosx_11_0_arm64.whl (macosx_11_0_arm64)
  • uploaded my_tool-1.2.3-py3-none-manylinux_2_28_x86_64.whl → https://upload.pypi.org/legacy/
  • uploaded my_tool-1.2.3-py3-none-macosx_11_0_arm64.whl → https://upload.pypi.org/legacy/
  • pypi publish complete for 'my-tool' (2 file(s))
```

## Configuration

```yaml
pypis:
  - id: main                              # CLI selector (--id=main)
    ids: [my-tool]                        # only this crate's binaries
    name: my-tool                         # PyPI project name (default: crate name)
    sdist: true                           # also `maturin sdist` (default: false)
    sdist_manifest: "pypi/"               # dir containing pyproject.toml (required with sdist)
    repository: "https://upload.pypi.org/legacy/"  # default; templated
    skip_existing: true                   # default; duplicate file ⇒ idempotent skip
    requires_python: ">=3.7"              # METADATA Requires-Python
    summary: "A demo CLI"                 # default: metadata.description
    description: |                        # long METADATA body (default: summary)
      Renders on the PyPI project page.
    homepage: "https://example.com"       # default: metadata.homepage
    license: MIT                          # default: metadata.license
    keywords: [cli, rust]
    classifiers:
      - "Programming Language :: Rust"
      - "Environment :: Console"
    token: "{{ .Env.MY_PYPI_TOKEN }}"     # default: $PYPI_TOKEN, then $MATURIN_PYPI_TOKEN
    skip: false                           # bool or template
    if: "{{ not .IsNightly }}"            # template-conditional gate
    required: true                        # default; failure aborts the release
    retain_on_rollback: false             # default
```

| Field | Default | Purpose |
|---|---|---|
| `id` | — | CLI selector for `--id=...` |
| `ids` | all crates | Only include binaries built from these crates |
| `targets` | all built | Target-triple allowlist: build wheels only for these triples. See [Publishing a subset of targets](#publishing-a-subset-of-targets) |
| `name` | crate name | PyPI project name; any PEP 508 form (`My.Tool`, `my_tool`) — PyPI normalizes per PEP 503, wheel filenames escape per PEP 427 |
| `sdist` | `false` | Also build + upload a source distribution via `maturin sdist` |
| `sdist_manifest` | — | Templated. Directory containing `pyproject.toml`; **required** when `sdist: true` |
| `repository` | `https://upload.pypi.org/legacy/` | Templated upload endpoint |
| `skip_existing` | `true` | Treat the index's already-exists rejection as an idempotent skip |
| `requires_python` | — | `Requires-Python` specifier (pip honors it during resolution) |
| `summary` | `metadata.description` | One-line METADATA `Summary` |
| `description` | falls back to `summary` | Long description (the PyPI project page body) |
| `homepage` | `metadata.homepage` | Emitted as `Project-URL: Homepage` |
| `license` | `metadata.license` | METADATA `License` |
| `keywords` | — | Comma-joined METADATA `Keywords` |
| `classifiers` | — | One `Classifier:` header each |
| `token` | `$PYPI_TOKEN` → `$MATURIN_PYPI_TOKEN` | Templated API token |
| `skip` / `if` | — | Entry gating (bool/template; falsy `if` skips) |
| `required` | `true` | Whether failure fails the release |
| `retain_on_rollback` | `false` | Leave work in place on rollback |

## Versions: semver → PEP 440

PyPI only accepts PEP 440 versions, so the release's semver version is mapped — the same mapping maturin applies, so a project migrating from maturin-built wheels keeps identical versions on the index:

| semver | PEP 440 |
|---|---|
| `1.2.3` | `1.2.3` |
| `1.2.3-alpha.4` / `-alpha4` / `-a.4` / `-a4` | `1.2.3a4` |
| `1.2.3-beta.4` / `-beta4` / `-b.4` / `-b4` | `1.2.3b4` |
| `1.2.3-rc.1` / `-rc1` / `-c.1` / `-pre.1` / `-preview.1` | `1.2.3rc1` |
| `1.2.3-rc` (bare label, no number) | `1.2.3rc0` |
| `1.2.3-dev.9` / `-dev9` | `1.2.3.dev9` |
| `1.2.3+build.7` | `1.2.3+build.7` (local segment) |

The label is matched case-sensitively against the supported set, with both
dotted (`-rc.1`) and suffix (`-rc1`) number forms accepted:

| semver label(s) | PEP 440 segment |
|---|---|
| `alpha`, `a` | `a` |
| `beta`, `b` | `b` |
| `rc`, `c`, `pre`, `preview` | `rc` |
| `dev` | `.dev` |

A **bare label with no number defaults the number to `0`** (`1.2.3-rc` →
`1.2.3rc0`). A prerelease whose label is outside this set (e.g.
`-nightly.20260712`) has no faithful PEP 440 equivalent and is an **error**,
not a silent rename — uploading a version pip would order differently than
cargo does is worse than failing.

## Source distributions (`sdist`)

Anodizer never synthesizes a `pyproject.toml` — sdist consumers build from source, so the project must own a real maturin manifest:

```toml
# pypi/pyproject.toml
[build-system]
requires = ["maturin>=1.0,<2.0"]
build-backend = "maturin"

[project]
name = "my-tool"

[tool.maturin]
bindings = "bin"
manifest-path = "../Cargo.toml"
```

```yaml
pypis:
  - sdist: true
    sdist_manifest: "pypi/"
```

With `sdist: true`, `maturin` must be on `PATH` (surfaced by `anodizer preflight` / `anodizer tools` only when enabled) and the produced tarball uploads alongside the wheels with `filetype: sdist`. `SOURCE_DATE_EPOCH` is pinned from the run context so the tarball is reproducible.

## Authentication

The `auth` field selects between a stored API token and PyPI [Trusted
Publishing](https://docs.pypi.org/trusted-publishers/) (GitHub Actions OIDC):

| `auth` | Behaviour |
|---|---|
| `auto` *(default)* | A token when one is available, otherwise a Trusted-Publishing exchange when an OIDC context is present. Errors only when neither exists. |
| `token` | Always the token; never OIDC. |
| `oidc` | Always Trusted Publishing; never fall back to a token. Errors loudly if the OIDC request env is absent. |

### Token

HTTP Basic auth with the literal username `__token__` — a [PyPI API
token](https://pypi.org/help/#apitoken):

1. `pypis[].token` (templated) when set;
2. `$PYPI_TOKEN`;
3. `$MATURIN_PYPI_TOKEN` (so a project migrating from `maturin publish` keeps its existing secret name).

### Trusted Publishing (OIDC)

No stored secret. anodizer requests a GitHub Actions id-token (audience
`pypi`) and exchanges it at the index's `/_/oidc/mint-token` endpoint for a
short-lived upload token. Requires `id-token: write` on the release job and a
[Trusted Publisher](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/)
(or a *pending* publisher, for a brand-new project) configured on PyPI for
this repository and workflow. Supported for `pypi.org` and `test.pypi.org`
only — a custom index has no mint endpoint.

```yaml
pypis:
  - name: my-tool
    auth: oidc          # no PYPI_TOKEN secret needed
```

To rehearse against TestPyPI (token):

```yaml
pypis:
  - repository: "https://test.pypi.org/legacy/"
    token: "{{ .Env.TEST_PYPI_TOKEN }}"
```

## Re-runs and the one-way door

The upload API rejects any filename the index has ever seen. anodizer's semantics around that:

- **`skip_existing: true` (default)** — a duplicate rejection (`400 File already exists` / `409` / `403` re-upload text) logs `skipped '<file>' — already on <repository> (idempotent)` and continues. Re-publishing a tag after a partial failure uploads only the missing files.
- **`skip_existing: false`** — a duplicate is a hard error naming the fix (bump the version).
- **Changed bytes, same version** — impossible to ship. A changed file with a published name can never replace the original; fix forward to the next version.
- **Preflight** — `anodizer preflight` probes the index (`https://pypi.org/pypi/<name>/<version>/json`, or the custom repository's PEP 503 `/simple/<name>/` page) and *warns* when the version already appears, mirroring the run path's idempotent handling.

## Wheel contents

For a project `my-tool` at `1.2.3` targeting `x86_64-unknown-linux-gnu` (glibc floor 2.28):

```
my_tool-1.2.3-py3-none-manylinux_2_28_x86_64.whl
├── my_tool-1.2.3.data/scripts/my-tool        (the prebuilt binary, mode 0755)
└── my_tool-1.2.3.dist-info/
    ├── METADATA                              (Metadata-Version 2.1)
    ├── WHEEL                                 (Root-Is-Purelib: false, Tag: py3-none-…)
    └── RECORD                                (per-file sha256 + size)
```

Wheel bytes are **deterministic**: entries are written in sorted order, deflate-compressed, with every mtime pinned to the commit timestamp (or `SOURCE_DATE_EPOCH`), so two builds of the same commit are byte-identical.

## Nightlies

The PyPI publisher skips nightly runs: every nightly upload would permanently consume a version/filename on an index users resolve against. See [Nightlies](./nightlies.md).
