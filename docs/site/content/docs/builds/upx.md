+++
title = "UPX Compression"
description = "Compress binaries with UPX to reduce download size"
weight = 4
template = "docs.html"
+++

[UPX](https://upx.github.io/) (the Ultimate Packer for eXecutables) compresses
compiled binaries in-place, often reducing them to 30-50% of their original size.
Anodizer runs UPX immediately after the build stage and before archiving, so the
smaller binaries flow into your archives, checksums, and releases automatically.

## Minimal config

```yaml
upx:
  enabled: true
```

This compresses every binary artifact using the default `upx` binary on your
`PATH` with no extra flags. If `upx` is not installed, the stage logs a warning
and continues.

## Config fields

The `upx` key accepts a single object or an array of objects. Each object
supports the following fields:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier for this UPX config (used in log output) |
| `ids` | list of strings | none | Only compress binaries whose build `id` (or binary name) is in this list. Omit to compress all binaries. |
| `enabled` | bool or template string | none (treated as disabled) | Whether this config is active. Supports template strings for conditional evaluation (e.g., `"{{ IsRelease }}"`) |
| `binary` | string | `"upx"` | Path or name of the UPX executable |
| `compress` | string | none | Compression level: `"1"` through `"9"`, or `"best"` (maps to `--best`) |
| `lzma` | bool | `false` | Enable LZMA compression (`--lzma` flag) |
| `brute` | bool | `false` | Enable brute-force compression (`--brute` flag). Very slow but produces the smallest output. |
| `args` | list of strings | `[]` | Additional raw arguments passed to UPX after all other flags |
| `targets` | list of glob patterns | none | Only compress binaries built for matching target triples. Omit to compress all targets. |
| `required` | bool | `false` | When `true`, fail the build if the UPX binary is not found. When `false`, skip with a warning. |

The `enabled` field must be explicitly set to `true` (or a template that
renders `true` or `1`, in any letter case (see [bool-or-template values](@/docs/general/templates.md#bool-or-template-values))) for the config to take effect. An omitted or `null` `enabled`
field is treated as disabled.

## Examples

### Basic compression with best settings

```yaml
upx:
  enabled: true
  compress: best
  lzma: true
```

### Only compress Linux binaries

```yaml
upx:
  enabled: true
  compress: best
  targets:
    - "*-linux-*"
```

### Different settings per platform

Use an array to define multiple UPX configs that each target different
platforms:

```yaml
upx:
  - id: linux
    enabled: true
    compress: best
    lzma: true
    targets:
      - "x86_64-*-linux-*"
      - "aarch64-*-linux-*"

  - id: windows
    enabled: true
    compress: "9"
    targets:
      - "*-windows-*"
```

### Filter by build ID

If your project produces multiple binaries and you only want to compress
some of them:

```yaml
upx:
  enabled: true
  ids: ["myapp", "myapp-cli"]
  compress: best
```

### Require UPX in CI

```yaml
upx:
  enabled: true
  required: true
  compress: best
```

With `required: true`, the build fails immediately if `upx` is not found on
the system. This is useful in CI, where a missing tool should be a hard error.

### Custom UPX binary path

```yaml
upx:
  enabled: true
  binary: /usr/local/bin/upx
  args:
    - "--best"
    - "--lzma"
```

## Target filtering

The `targets` field accepts glob patterns matched against Rust target triples.
The `*` wildcard matches any sequence of characters. When `targets` is set,
only binaries whose target triple matches at least one pattern are compressed.
Binaries with no target metadata are skipped when a targets filter is present.

Common patterns:

| Pattern | Matches |
|---------|---------|
| `"*-linux-*"` | All Linux targets |
| `"*-windows-*"` | All Windows targets |
| `"*-apple-darwin"` | All macOS targets |
| `"x86_64-*"` | All x86_64 targets |
| `"aarch64-*"` | All AArch64/ARM64 targets |
| `"*"` | Everything (same as omitting `targets`) |

Note that UPX itself does not support every executable format. If UPX
encounters a binary it cannot compress (for example, a format it does not
recognize), it reports a known exception such as `CantPackException` or
`UnknownExecutableFormatException`. Anodizer treats these as warnings and
skips the binary, and the build goes on.

## Pipeline position

UPX runs as the second stage in the pipeline, immediately after the build
stage and before changelog generation and archiving:

```
build -> upx -> changelog -> archive -> ...
```

This means compressed binaries are what end up in your `.tar.gz`, `.zip`, and
other archive formats. Checksums, SBOMs, and signatures all reflect the
compressed binary sizes.

In `--split` mode (where build and release are separate steps), UPX is included
in the build-only pipeline, so the compressed artifacts are available for the
later release step.

## Parallel compression

Anodizer compresses matching binaries in parallel, bounded by the global
`--parallelism` setting. Each chunk of binaries is processed concurrently
using threads, matching the parallelism model used by other stages.

## Dry run

When running with `--dry-run`, the UPX stage logs what it would compress
(including the flags it would use) without actually invoking UPX or modifying
any files.
