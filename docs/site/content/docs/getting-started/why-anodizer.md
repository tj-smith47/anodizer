+++
title = "Why Anodizer"
description = "The Rust-native release features anodizer is built around."
weight = 5
template = "docs.html"
+++

The pipeline stages — build, archive, sign, release, publish, announce — are table stakes.
What follows is what anodizer does *because* it's Rust-first: it understands Cargo workspaces,
lockfiles, and crates.io, and it proves its own output is reproducible.

## Workspace-native releases

Single crates and multi-crate workspaces use the same config. Each crate can release on its own
cadence with its own tag prefix, and anodizer keeps cross-crate version specs in sync.

```yaml
workspaces:
  - crates:
      - name: my-core
        tag_template: "my-core-v{{ Version }}"
      - name: my-cli
        tag_template: "my-cli-v{{ Version }}"
        depends_on: [my-core]
```

See [Monorepo / workspaces](@/docs/advanced/monorepo.md).

## Cargo- and lockfile-aware versioning

`anodizer tag` reads Conventional Commits, bumps `Cargo.toml` **and** `Cargo.lock`, commits,
tags, and (with `--push`) pushes the bump commit and tag atomically — so a tag never points at a
commit missing from the branch.

```bash
anodizer tag --push          # bump + tag + push, no orphaned commit
anodizer bump minor          # bump versions in a PR-first workflow
```

See [Auto-tagging](@/docs/advanced/auto-tagging.md).

## crates.io, ordered correctly

Workspace crates publish in dependency order, and anodizer waits for each dependency to appear
on the sparse index before publishing its dependents — no racing propagation.

```yaml
publish:
  cargo:
    wait_for_workspace_deps: true
    required: true          # fail the release if cargo publish fails
```

## Publisher resilience

Every publisher carries explicit rollback semantics. When a publisher fails, `on_error` hooks
fire with structured context — you get a shell command with `.Publisher`, `.Error`, `.Tag`,
`.Required`, and `.RolledBack` in scope.

```yaml
defaults:
  publish:
    on_error:
      - cmd: "echo '{{ .Publisher }} failed on {{ .Tag }}: {{ .Error }}'"

publish:
  cargo:
    retain_on_rollback: true   # crates.io is permanent — skip rollback here
  schemastore:
    retain_on_rollback: true
```

`retain_on_rollback` is intentional for publishers that are not reversible: once a crate is on
crates.io you cannot un-publish it, so rollback must skip that step rather than fail.
Per-crate hooks fire before defaults, so per-publisher overrides compose cleanly with a
workspace-wide catch-all.

## Reproducible — and verified

Artifacts are deterministic by default. The determinism harness rebuilds in a hermetic worktree
and byte-compares the output, so reproducibility is proven, not assumed.

```bash
anodizer check determinism
```

The harness runs in CI across the full OS matrix (Linux, macOS, Windows) on every release — via
`determinism: 'true'` in the GitHub Action — and is sharded per workspace crate for large repos.

See [Determinism](@/docs/advanced/determinism.md) and
[Reproducible builds](@/docs/advanced/reproducible-builds.md).

## Zero-config cross-compilation

musl, glibc, Windows, and macOS targets build via `cargo-zigbuild` or `cross` without
per-target toolchain setup.

```yaml
crates:
  - name: my-cli
    builds:
      - targets:
          - x86_64-unknown-linux-musl
          - aarch64-apple-darwin
          - x86_64-pc-windows-msvc
```

See [Cross-compilation](@/docs/builds/cross-compilation.md).

## Changelog as a first-class command

`anodizer changelog` renders changelogs from git history without requiring a separate tool.
Three output formats cover every use case: Keep a Changelog, GitHub release body, and machine-
readable JSON.

```bash
anodizer changelog                     # render from latest tag to HEAD
anodizer changelog v0.4.0..v0.5.0     # explicit range
anodizer changelog --format release-notes --write   # write + use as release body
```

In a monorepo, each crate gets its own `CHANGELOG.md`:

```yaml
changelog:
  files:
    per_crate: true    # crates/my-core/CHANGELOG.md, crates/my-cli/CHANGELOG.md, ...
```

## Version-file sync at tag time

`version_files` rewrites version strings in any tracked file when a tag is cut — docs,
Helm chart values, install scripts — atomically with the version-bump commit.

```yaml
version_files:
  - docs/installation.md
  - chart/my-app/Chart.yaml
```

No scripting required. Any file containing the previous version string gets the new one.

## Nightly releases

A `nightly:` block enables scheduled prerelease builds without creating a permanent tag in your
semver history. The action's `from-branch` input lets you run the full pipeline against an
unreleased branch — useful for proving a publisher before the feature ships.

```yaml
nightly:
  name_template: "{{ ProjectName }}-nightly"
  tag_name: nightly
```

```yaml
# .github/workflows/nightly.yml
- uses: tj-smith47/anodizer-action@v1
  with:
    from-branch: my-feature-branch    # build anodizer from this branch
    args: release --nightly --no-preflight
    apk-private-key: ${{ secrets.APK_PRIVATE_KEY }}
```

## GoReleaser-compatible template language

Templates use the Tera engine with a preprocessor that auto-translates GoReleaser's
`text/template` syntax — `{{ .Field }}`, `eq`/`ne`/`and`/`or`/`not`, positional function
calls, `len`, `printf`. Existing GoReleaser template strings work without modification.

anodizer extends the template surface with hash helpers (`sha256`, `blake3`, `md5`),
`readFile`/`mustReadFile`, time formatting, `reReplaceAll`, `urlPathEscape`, and `mdv2escape`.

```yaml
metadata:
  description: "{{ readFile \"README.md\" | truncate(200) }}"
  mod_timestamp: "{{ CommitTimestamp }}"
```

## Rust-ecosystem niceties

- **Generated crate READMEs** kept in sync with a template.
- **`cargo-binstall` metadata** derived from your build config automatically.
- **AppImage packaging** for Linux — produces self-contained `.AppImage` bundles.
- **MCP publisher** — publishes your tool's JSON schema to the Model Context Protocol registry.
- **SchemaStore publisher** — submits your config schema to
  [SchemaStore.org](https://www.schemastore.org/json/) for IDE autocompletion.
- **Build from a branch** in CI via the GitHub Action — dogfood a release pipeline before tagging.
