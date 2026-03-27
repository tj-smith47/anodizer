+++
title = "From GoReleaser"
description = "Migrate from GoReleaser to anodize"
weight = 1
template = "docs.html"
+++

If you're coming from GoReleaser, anodize will feel familiar. The config structure, CLI verbs, and template vocabulary are intentionally similar.

## Config mapping

| GoReleaser | Anodize | Notes |
|------------|---------|-------|
| `project_name` | `project_name` | Identical |
| `builds` | `crates[].builds` | Nested under crate config |
| `archives` | `crates[].archives` | Same fields, nested under crate |
| `checksum` | `defaults.checksum` or `crates[].checksum` | Can be global or per-crate |
| `changelog` | `changelog` | Same structure |
| `release` | `crates[].release` | Nested under crate |
| `brews` | `crates[].publish.homebrew` | Renamed; nested under publish |
| `scoop` | `crates[].publish.scoop` | Nested under publish |
| `dockers` | `crates[].docker` | Nested under crate |
| `signs` | `signs` | Top-level, same structure |
| `nfpms` | `crates[].nfpm` | Nested under crate |
| `announces` | `announce` | Same structure |
| `snapshot` | `snapshot` | Identical |
| `env` | `env` | Identical |
| `before.hooks` | `before.hooks` | Identical |

## Template syntax

Both GoReleaser and anodize template styles work:

```yaml
# GoReleaser style (works in anodize):
name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"

# Native Tera style:
name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## Key differences

1. **Crate-centric config**: In GoReleaser, builds/archives/releases are top-level arrays. In anodize, they're nested under `crates[]` to support workspace-based releases.

2. **Cross-compilation**: GoReleaser uses `GOOS`/`GOARCH`. Anodize uses Rust target triples (`x86_64-unknown-linux-gnu`) with auto-detected cross-compilation strategy.

3. **Template engine**: GoReleaser uses Go templates. Anodize uses Tera (Jinja2-like). The GoReleaser `{{ .Field }}` syntax is supported for compatibility, but Tera's native syntax offers more features (pipes, filters, loops).

4. **Package manager names**: `brews` → `publish.homebrew`, `scoop` → `publish.scoop`.

## Migration steps

1. Install anodize: `cargo install anodize`
2. Run `anodize init` to generate a starter config from your `Cargo.toml`
3. Copy relevant settings from your `.goreleaser.yaml` into `.anodize.yaml`, adjusting for the nested crate structure
4. Run `anodize check` to validate
5. Run `anodize release --dry-run` to verify the pipeline
