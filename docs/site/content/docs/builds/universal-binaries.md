+++
title = "Universal Binaries"
description = "Create macOS universal binaries (x86_64 + aarch64)"
weight = 3
template = "docs.html"
+++

macOS universal binaries (also called "fat binaries") combine native code for both `x86_64` (Intel) and `aarch64` (Apple Silicon) into a single executable. A universal binary runs natively on both architectures without Rosetta translation, giving users a single download that works on any Mac.

Anodizer creates universal binaries by running Apple's `lipo` tool after both architecture-specific builds have completed.

## Config

Add `universal_binaries` to a crate definition:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    universal_binaries:
      - replace: true
```

### Options

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ .ProjectName }}` | Template for the output filename. Supports all standard template variables (`ProjectName`, `Version`, etc.). |
| `replace` | bool | `false` | When `true`, remove the individual per-architecture binaries from the artifact registry. Downstream stages (archives, release, publishers) will only see the universal binary. |
| `ids` | list of strings | all binaries | Filter which binaries to combine. Only artifacts whose binary name matches an entry in this list are considered. Useful when a crate produces multiple binaries and you only want universal builds for some of them. |
| `hooks` | object | none | Pre/post hooks to run around universal binary creation. Uses the same `pre`/`post` hook format as build hooks. |
| `mod_timestamp` | string | none | Override the output file's modification timestamp for reproducible builds. Supports templates (e.g., `{{ CommitDate }}`). |

## Examples

### Basic usage

Produce a universal binary and keep the per-arch binaries as well:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
          - x86_64-unknown-linux-gnu
    universal_binaries:
      - {}
```

### Replace per-arch binaries

Ship only the universal binary for macOS, removing the individual `x86_64` and `aarch64` artifacts from archives and releases:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    universal_binaries:
      - replace: true
```

### Custom output name

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    universal_binaries:
      - name_template: "{{ ProjectName }}-{{ Version }}-darwin-universal"
        replace: true
```

### Filter by binary name

When a crate produces multiple binaries, combine only specific ones:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
      - binary: myapp-cli
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    universal_binaries:
      - ids:
          - myapp
          - myapp-cli
```

### Hooks

Run commands before and after the `lipo` step:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    universal_binaries:
      - hooks:
          pre:
            - cmd: echo "Creating universal binary..."
          post:
            - cmd: codesign --sign "Developer ID" dist/myapp_darwin_all/myapp
```

## How it works

1. Anodizer builds both `x86_64-apple-darwin` and `aarch64-apple-darwin` targets as normal build artifacts.
2. After all builds complete, anodizer iterates over each crate's `universal_binaries` entries.
3. For each entry, it locates the matching `aarch64-apple-darwin` and `x86_64-apple-darwin` binary artifacts. If the `ids` filter is set, only binaries whose name appears in the list are considered.
4. If both architecture binaries are found, anodizer runs `lipo -create -output <output> <arm64> <x86_64>`.
5. The universal binary is placed in `dist/<crate_name>_darwin_all/` and registered as an artifact with kind `UniversalBinary` and target `darwin-universal`.
6. When `replace: true`, the individual per-architecture artifacts are removed from the registry so downstream stages (archives, checksums, release uploads, package publishers) only see the universal binary.

If either architecture binary is missing, anodizer logs a warning and skips universal binary creation for that crate rather than failing the build.

## Limitations

- **macOS only.** Universal binaries are an Apple concept. The `lipo` tool is required and is only available on macOS (ships with Xcode Command Line Tools).
- **Requires both architectures.** Both `x86_64-apple-darwin` and `aarch64-apple-darwin` must be listed in `targets` and must build successfully. If either is missing, the universal binary step is skipped with a warning.
- **lipo must be on PATH.** If `lipo` is not found and `universal_binaries` is configured, anodizer will fail with an error. Install Xcode Command Line Tools (`xcode-select --install`) to get `lipo`.
- **CI considerations.** GitHub Actions macOS runners include `lipo`. If you cross-compile macOS targets on Linux, you will need to transfer the binaries to a macOS runner (or use a macOS cross-lipo tool) for the universal binary step.
