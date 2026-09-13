+++
title = "Reproducible Builds"
description = "Produce bit-for-bit reproducible release artifacts"
weight = 5
template = "docs.html"
+++

Reproducible builds ensure that building the same source at the same commit always produces
identical artifacts. This matters for supply-chain security (auditors can verify that a
published binary matches the source) and for caching (identical inputs yield identical outputs).

Anodizer provides several mechanisms to eliminate non-determinism: the `reproducible` flag on
build configs, the `mod_timestamp` field on builds and packaging stages, deterministic archive
ordering, and the `CommitTimestamp` template variable.

## The `reproducible` flag

Set `reproducible: true` on a build config to enable automatic determinism for Rust
compilation. When enabled, anodizer does three things:

1. **Sets `SOURCE_DATE_EPOCH`** in the build environment to the commit timestamp, giving
   `rustc` and any build scripts a stable reference time.
2. **Injects `--remap-path-prefix`** into `RUSTFLAGS`, rewriting your local working
   directory to `/build` so that absolute paths embedded in debug info do not vary across
   machines.
3. **Sets binary mtime** to the `SOURCE_DATE_EPOCH` value after compilation, so the output
   file's modification time is deterministic.

```yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
        reproducible: true
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
```

If `SOURCE_DATE_EPOCH` is already set in the environment (e.g., by your CI system), anodizer
preserves the existing value rather than overwriting it. The `--remap-path-prefix` flag is
appended to any existing `RUSTFLAGS` rather than replacing them.

When neither `SOURCE_DATE_EPOCH` nor `CommitTimestamp` can be resolved to a valid epoch,
anodizer prints a warning and skips the mtime step.

## The `mod_timestamp` field

The `mod_timestamp` field gives explicit control over the modification timestamp applied to
output files. It is available on builds, universal binaries, and every packaging stage
(archives are handled via `builds_info`, see below). The value is a template string that is
rendered at build time, so you can use any template variable.

`mod_timestamp` accepts two formats:

- **Unix epoch seconds** (e.g., `"1704067200"`)
- **RFC 3339 / ISO 8601** (e.g., `"2024-01-01T00:00:00Z"`)

The most common pattern is to use the commit timestamp:

```yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
        mod_timestamp: "{{ CommitTimestamp }}"
        targets:
          - x86_64-unknown-linux-gnu
```

When both `reproducible: true` and `mod_timestamp` are set on the same build, `mod_timestamp`
takes precedence -- it is applied after the reproducible mtime step and overwrites it.

### Where `mod_timestamp` is supported

| Config section | Effect |
|----------------|--------|
| `builds[].mod_timestamp` | Sets mtime on the compiled binary |
| `universal_binaries[].mod_timestamp` | Sets mtime on the merged universal binary |
| `snapcrafts[].mod_timestamp` | Sets mtime on the snap output |
| `dmgs[].mod_timestamp` | Sets mtime on the DMG image |
| `msis[].mod_timestamp` | Sets mtime on the MSI installer and rendered .wxs |
| `pkgs[].mod_timestamp` | Sets mtime on the macOS PKG installer |
| `nsis[].mod_timestamp` | Sets mtime on the NSIS installer |
| `appbundles[].mod_timestamp` | Sets mtime recursively on the .app bundle |
| `flatpaks[].mod_timestamp` | Sets mtime on the Flatpak bundle |
| `metadata.mod_timestamp` | Sets mtime on `metadata.json` and `artifacts.json` |

## The `CommitTimestamp` template variable

`CommitTimestamp` is a built-in template variable set to the unix epoch timestamp of the HEAD
commit (the author date). It is available in all template contexts and is the recommended
value for `mod_timestamp`:

```yaml
metadata:
  mod_timestamp: "{{ CommitTimestamp }}"
```

After rendering, `{{ CommitTimestamp }}` produces a string like `"1700000000"` which
`mod_timestamp` parses as unix epoch seconds.

A related variable, `CommitDate`, provides the same instant as an ISO 8601 string
(e.g., `"2026-03-25T10:30:00+00:00"`).

## Deterministic archives

The archive stage produces deterministic output in two ways:

### Entry ordering

Archive entries are sorted alphabetically by their in-archive path before being written. This
ensures that the same set of files always produces the same archive regardless of filesystem
enumeration order.

### Fixed timestamps via `builds_info`

When any crate has `reproducible: true`, the archive stage automatically uses
`CommitTimestamp` as the mtime for every entry in tar and zip archives. If `reproducible` is
not set, the archive stage falls back to the `SOURCE_DATE_EPOCH` environment variable if
present.

You can also control archive entry metadata explicitly with `builds_info`. The
fragments below show the `archives:` block on its own; it sits under
`crates[].archives:` (or `defaults.archives:`), as the examples above spell out.

```yaml
archives:
  - formats: [tar.gz]
    builds_info:
      owner: root
      group: root
      mode: "0755"
      mtime: "2024-01-01T00:00:00Z"
```

The `builds_info` fields are:

| Field | Type | Description |
|-------|------|-------------|
| `owner` | string | File owner name in the archive header |
| `group` | string | File group name in the archive header |
| `mode` | octal string | Permission bits (e.g., `"0755"`) |
| `mtime` | RFC 3339 string | Modification time for archive entries |

Per-file info can also be set on individual `files` entries:

```yaml
archives:
  - formats: [tar.gz]
    files:
      - src: LICENSE
        dst: LICENSE
        info:
          mode: "0644"
```

## Source archives

Source archives are created with `git archive`, which only includes tracked files and
naturally excludes build artifacts and untracked files. The timestamps in a `git archive`
output are determined by git itself (using the commit tree timestamps), so source archives
are inherently more reproducible than binary archives.

Anodizer does not currently apply `mod_timestamp` to source archives -- the `git archive`
output is used as-is.

## Full example

A complete configuration combining all reproducibility features:

```yaml
project_name: myapp

metadata:
  mod_timestamp: "{{ CommitTimestamp }}"

crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
        reproducible: true
        mod_timestamp: "{{ CommitTimestamp }}"
        targets:
          - x86_64-unknown-linux-gnu
          - x86_64-unknown-linux-musl
          - aarch64-unknown-linux-gnu
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    archives:
      - formats: [tar.gz]
        builds_info:
          owner: root
          group: root
          mode: "0755"
```

## Limitations and caveats

- **`reproducible: true` is Rust-specific.** It sets `SOURCE_DATE_EPOCH` and injects
  `--remap-path-prefix` into `RUSTFLAGS`. If your build uses other compilers or build
  systems via pre/post hooks, you may need to handle those separately.

- **Compression non-determinism.** Some compression libraries (notably gzip) embed
  timestamps or OS identifiers in the compressed stream header. Anodizer uses libraries
  that produce deterministic output, but if you shell out to external compression tools,
  verify they do the same.

- **Proc macros and build scripts.** Rust proc macros and `build.rs` scripts can embed
  non-deterministic data (e.g., `env!("HOME")`, file modification times). Audit your
  dependency tree for such usage if full bit-for-bit reproducibility is required.

- **Cross-platform differences.** Even with all timestamps pinned, binaries compiled on
  different operating systems or with different toolchain versions will differ. Reproducible
  builds guarantee identical output given identical inputs -- the same OS, toolchain, and
  source.

- **Source archives use git timestamps.** The `mod_timestamp` field has no effect on source
  archives produced by `git archive`. To control source archive timestamps, use git's
  built-in mechanisms or set `SOURCE_DATE_EPOCH` before running `git archive`.

- **`mod_timestamp` overrides `reproducible`.** When both are set on a build, the explicit
  `mod_timestamp` value wins. This is intentional -- it lets you use `reproducible: true`
  for the env var injection while still controlling the exact timestamp.
