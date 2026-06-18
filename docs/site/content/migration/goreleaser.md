+++
title = "From GoReleaser"
description = "Migrate from GoReleaser to anodizer"
weight = 1
template = "docs.html"
+++

If you're coming from GoReleaser, anodizer will feel familiar. The config structure, CLI verbs, and template vocabulary are intentionally similar.

## What you gain

GoReleaser builds Rust binaries, but anodizer is Rust-first — it understands the Cargo ecosystem your release lives in:

- **Cargo workspaces** as a first-class unit: per-crate release cadences, per-crate tags, a tag resolver, and cross-crate version syncing.
- **`Cargo.toml` / `Cargo.lock`-aware version bumps** (`anodizer tag` / `bump`) that commit, tag, and push the bump atomically.
- **crates.io publishing** with dependency-aware ordering and sparse-index polling.
- **A determinism harness** that rebuilds and byte-compares artifacts to prove reproducibility.
- **Zero-config Rust cross-compilation** via `cargo-zigbuild` / `cross`.

Everything else maps over directly — the table below is the field-by-field translation.

## Config mapping

| GoReleaser | Anodizer | Notes |
|------------|---------|-------|
| `project_name` | `project_name` | Identical |
| `builds` | `crates[].builds` | Nested under crate config |
| `archives` | `crates[].archives` | Same fields, nested under crate |
| `checksum` | `defaults.checksum` or `crates[].checksum` | Can be global or per-crate |
| `changelog` | `changelog` | Same structure |
| `release` | `crates[].release` | Nested under crate |
| `brews` | `homebrew_casks:` (top-level) or `crates[].publish.homebrew_cask` | **Deprecated upstream in GoReleaser v2.16.** `publish.homebrew` (Formula) still parses with a deprecation warning; see [the `brews → homebrew_casks` migration](#brews-homebrew_casks) below. |
| `scoop` | `crates[].publish.scoop` | Nested under publish |
| `mcp` | `mcp` | Identical top-level key. The deprecated nested `mcp.github:` block from older GoReleaser configs collapses to top-level `mcp.*` fields in anodizer (matches upstream's current recommendation). See [MCP registry](@/docs/publish/mcp-registry.md) |
| `dockers` | `crates[].dockers_v2` | Nested under crate (multi-arch buildx; V2-only) |
| `signs` | `signs` | Top-level, same structure |
| `nfpms` | `crates[].nfpms` | Nested under crate (singular `nfpm` accepted as a legacy alias) |
| `announces` | `announce` | Same structure |
| `snapshot` | `snapshot` | Identical |
| `env` | `env` | Identical |
| `before.hooks` | `before.hooks` | Identical |

## Template syntax

Both GoReleaser and anodizer template styles work:

```yaml
# GoReleaser style (works in anodizer):
name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"

# Native Tera style:
name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## Key differences

1. **Crate-centric config**: In GoReleaser, builds/archives/releases are top-level arrays. In anodizer, they're nested under `crates[]` to support workspace-based releases.

2. **Cross-compilation**: GoReleaser uses `GOOS`/`GOARCH`. Anodizer uses Rust target triples (`x86_64-unknown-linux-gnu`) with auto-detected cross-compilation strategy.

3. **Template engine**: GoReleaser uses Go templates. Anodizer uses Tera (Jinja2-like). The GoReleaser `{{ .Field }}` syntax is supported for compatibility, but Tera's native syntax offers more features (pipes, filters, loops).

4. **Package manager names**: `brews` → `homebrew_casks:` (top-level array — Formula is deprecated upstream as of GoReleaser v2.16, see [brews → homebrew_casks](#brews-homebrew_casks) below), `scoop` → `publish.scoop`. MCP keeps the same top-level `mcp:` key — see [MCP registry](@/docs/publish/mcp-registry.md) for the nested `mcp.github:` collapse.

5. **Tag sorting**: Anodizer adds a `smartsemver` mode for `git.tag_sort` that automatically filters prerelease tags when computing the previous tag for changelogs. This prevents the empty-changelog problem that occurs when shipping `v1.0.0` after `v1.0.0-rc.1` — GoReleaser would see `v1.0.0-rc.1` as the previous tag and produce an empty diff. Set `git.tag_sort: smartsemver` to opt in.

## Migration steps

1. Install anodizer: `cargo install anodizer`
2. Run `anodizer init` to generate a starter config from your `Cargo.toml`
3. Copy relevant settings from your `.goreleaser.yaml` into `.anodizer.yaml`, adjusting for the nested crate structure
4. Run `anodizer check` to validate
5. Run `anodizer release --dry-run` to verify the pipeline
6. Replace the `goreleaser/goreleaser-action` step in CI with [`tj-smith47/anodizer-action`](@/docs/ci/anodizer-action.md)

## CI workflow replacement

Where a GoReleaser workflow looks like:

```yaml
- uses: goreleaser/goreleaser-action@v6
  with:
    version: latest
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

The anodizer equivalent is:

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    auto-install: true          # auto-installs nfpm, cosign, etc. from .anodizer.yaml
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

`auto-install: true` parses `.anodizer.yaml` and installs pipeline dependencies (nfpm for linux packages, cosign for signing, zig/cargo-zigbuild for cross-compilation, ...) — the anodizer action's equivalent of GoReleaser's bundled Go-native implementations. See [anodizer-action reference](@/docs/ci/anodizer-action.md) for all inputs.

## v0.5.x → v0.6.x: installer default name template changed {#v05x-v06x-installer-name}

The `pkg`, `nsis`, `msi`, and `dmg` stages now default to
`'{{ ProjectName }}_{{ Arch }}'` (matching GoReleaser's convention) instead of
the prior `'{{ ProjectName }}_{{ Version }}_{{ Arch }}.<ext>'`. If you relied on
the version being part of the installer filename, pin `name:` explicitly in each
stage config block:

```yaml
# Pin to preserve the old naming:
pkgs:
  - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"

msis:
  - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"

nsis:
  - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"

dmgs:
  - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"
```

## `brews` → `homebrew_casks` {#brews-homebrew_casks}

GoReleaser **v2.16** ([release blog](https://goreleaser.com/blog/goreleaser-v2.16/)) deprecated the `brews:` (Homebrew Formula) publisher. Quoting upstream:

> Migrate to `homebrew_casks`, which is the right tool for the job: it's how Homebrew expects pre-compiled binaries to be distributed, and it gets all the new features (completion generation, post-install hooks, and so on).

Anodizer mirrors the deprecation: `publish.homebrew` still parses (so existing configs do not break), but a `DEPRECATION:` warning is emitted at config-load time. New configs should write `homebrew_casks:` (top-level) or `publish.homebrew_cask:` (per-crate) directly.

### Before (Formula — deprecated)

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
        directory: Formula           # default for Formula
        description: "My CLI tool"
        homepage: https://myapp.io
        license: MIT
        dependencies:
          - name: openssl
        conflicts:
          - other-tool
        caveats: "Run `myapp init` to set up."
        commit_msg_template: "Brew formula update for {{ ProjectName }} version {{ Tag }}"
```

### After (Cask — preferred)

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
    directory: Casks                 # default for Cask
    description: "My CLI tool"
    homepage: https://myapp.io
    binaries:
      - name: myapp                  # the binary stub Homebrew symlinks into /usr/local/bin
    dependencies:
      - cask: other-cask             # cask: or formula: target
    conflicts:
      - cask: other-tool
    caveats: "Run `myapp init` to set up."
    commit_msg_template: "Brew cask update for {{ ProjectName }} version {{ Tag }}"
    # Cask-exclusive surface (no Formula equivalent):
    completions:
      bash: "completions/myapp.bash"
      zsh:  "completions/_myapp"
      fish: "completions/myapp.fish"
    hooks:
      post:
        install: |
          system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{staged_path}/myapp"]
    generate_completions_from_executable:
      executable: "bin/myapp"
      args: ["completions"]
      base_name: "myapp"
      shell_parameter_format: "clap"
      shells: ["bash", "zsh", "fish"]
```

### Field mapping

| Formula field (`publish.homebrew`) | Cask field (`homebrew_casks[]` / `publish.homebrew_cask`) | Notes |
|---|---|---|
| `repository`, `commit_author`, `commit_msg_template`, `directory`, `name` | Same names | Identical semantics |
| `description`, `homepage`, `license`, `caveats`, `custom_block`, `service`, `skip_upload` | Same names | Identical semantics |
| `ids` | `ids` | Same — artifact-id filter |
| `url_template` | `url_template` (or structured `url:` with `verified`, `using`, `headers`, etc.) | Cask gives a richer structured `url:` block |
| `url_headers` | `url.headers` | Cask uses the structured form |
| `download_strategy` | `url.using` | Cask uses the structured form |
| `dependencies[].name` | `dependencies[].cask` or `dependencies[].formula` | Cask requires the dependency kind |
| `dependencies[].os`, `dependencies[].type`, `dependencies[].version` | not yet on Cask | Upstream HomebrewCask omits these — see [Homebrew Cask Cookbook depends_on](https://docs.brew.sh/Cask-Cookbook#stanza-depends_on) |
| `conflicts[]` (string or `{name, because}`) | `conflicts[].cask` / `conflicts[].formula` | Cask conflicts use the structured form |
| `install`, `extra_install`, `post_install`, `test` | `hooks.pre.install`, `hooks.post.install` (preflight / postflight) + `generate_completions_from_executable` | Cask uses Ruby DSL hooks instead of formula install/test blocks |
| `plist`, `service` | `service` | Cask has no `plist` directive; use `service` |
| `amd64_variant`, `arm_variant` | _(no upstream equivalent on `homebrew_casks`)_ | Variant filters were Formula-only on GoReleaser |
| `custom_require` | _(not applicable)_ | Custom Ruby `require` is a Formula download-strategy concern; Casks use `url.using` instead |
| `cask:` (sub-block under `publish.homebrew`) | _(top-level field — promote the sub-block to its own entry)_ | The legacy `publish.homebrew.cask:` sub-block was a transitional shape; write `homebrew_casks:` or `publish.homebrew_cask:` directly |
