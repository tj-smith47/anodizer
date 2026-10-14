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
| `id` | string | none | Identifier recorded on the universal binary artifact as its `id`. When `ids` is unset, it is also the build id the entry combines |
| `name_template` | string | `{{ .ProjectName }}` | Template for the output filename. Supports all standard template variables (`ProjectName`, `Version`, etc.). |
| `replace` | bool | `false` | When `true`, remove the individual per-architecture binaries from the artifact registry. Downstream stages (archives, release, publishers) will only see the universal binary. |
| `ids` | list of strings | `[id]`, else `[project_name]` when a build of the crate carries that id, else `[<crate name>]` | Build ids to combine: only binaries from `builds[]` entries whose `id` is in the list are considered. A build without an `id` takes its binary name as its id. |
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

### Several binaries

One entry merges one binary: the builds it selects must give one `x86_64-apple-darwin` and
one `aarch64-apple-darwin` slice. A crate with two binaries gets one entry
per binary:

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
      - id: myapp            # combines the build whose id is myapp
        name_template: myapp
      - id: myapp-cli
        name_template: myapp-cli
```

Each entry needs its own `id`, and with `ids` unset the `id` is the build id
it combines.

Listing both ids in one entry puts two slices on each target, and the step
fails as described under [How it works](#how-it-works).

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

### Two amd64 levels of one binary

A crate that also builds a `v3` amd64 binary for macOS gives each build its
own `id` and names the one the universal binary takes:

```yaml
crates:
  - name: myapp
    builds:
      - id: myapp
        binary: myapp
        targets:
          - x86_64-apple-darwin
          - aarch64-apple-darwin
      - id: myapp-v3
        binary: myapp
        amd64_variant: v3
        targets:
          - x86_64-apple-darwin
    universal_binaries:
      - ids: [myapp]
```

## How it works

1. Anodizer builds both `x86_64-apple-darwin` and `aarch64-apple-darwin` targets as normal build artifacts.
2. After all builds complete, anodizer iterates over each crate's `universal_binaries` entries.
3. For each entry, it locates the matching `aarch64-apple-darwin` and `x86_64-apple-darwin` binary artifacts. If the `ids` filter is set, only binaries whose name appears in the list are considered.
   Two binaries on one of those targets (a baseline and a `v3` amd64 build of the same binary) are two slices of one CPU type, which a universal binary cannot hold, so the step fails instead of merging whichever was built first. The error names both paths with their `builds[].id`, adding the amd64 level (`v1` when unset) on an `x86_64-apple-darwin` slice; an `aarch64-apple-darwin` slice has no amd64 level. Give each build its own `builds[].id` and narrow `ids` to one of them; two builds without an `id` share the binary name as their id, which `ids` cannot tell apart.
4. If both architecture binaries are found, anodizer runs `lipo -create -output <output> <arm64> <x86_64>`.
5. The universal binary is placed in `dist/<crate_name>_darwin_all/` and registered as an artifact with kind `UniversalBinary` and target `darwin-universal`.
6. When `replace: true`, the individual per-architecture artifacts are removed from the registry so downstream stages (archives, checksums, release uploads, package publishers) only see the universal binary.

If either architecture binary is missing, anodizer notes it at verbose (`-v`) and skips universal binary creation for that crate rather than failing the build.

## Limitations

- **macOS only.** Universal binaries are an Apple concept. The `lipo` tool is required and is only available on macOS (ships with Xcode Command Line Tools).
- **Requires both architectures.** Both `x86_64-apple-darwin` and `aarch64-apple-darwin` must be listed in `targets` and must build successfully. If either is missing, the universal binary step is skipped with a verbose note.
- **lipo must be on PATH.** If `lipo` is not found and `universal_binaries` is configured, anodizer will fail with an error. Install Xcode Command Line Tools (`xcode-select --install`) to get `lipo`.
- **CI considerations.** GitHub Actions macOS runners include `lipo`. If you cross-compile macOS targets on Linux, you will need to transfer the binaries to a macOS runner (or use a macOS cross-lipo tool) for the universal binary step.
