+++
title = "Why Anodizer"
description = "The Rust-native release features anodizer is built around."
weight = 5
template = "docs.html"
+++

The pipeline stages — build, archive, sign, release, publish, announce — are table stakes.
What follows is what anodizer does *because* it's Rust-first: it understands Cargo workspaces,
lockfiles, and crates.io, and it proves its own output is reproducible.

Every capability on this page is dogfooded — anodizer releases itself with it, and the proof
is a clickable artifact, not a test name. See
[What works (with proof)](@/dogfooding/_index.md) for the live matrix.

## Workspace-native releases

Single crates and multi-crate workspaces use the same config. Each crate can release on its own
cadence with its own tag prefix, and anodizer keeps cross-crate version specs in sync.

```yaml
workspaces:
  - name: tools
    crates:
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
defaults:
  publish:
    cargo:
      wait_for_workspace_deps: { enabled: true }
      required: true        # fail the release if cargo publish fails
```

## npm packages with automatic auth selection

The `npms[]` publisher ships your CLI to npm as platform-specific packages and picks the right
authentication mode *per package, at publish time*. With `auth: auto`, anodizer probes whether
the package already exists: established packages publish via OIDC Trusted Publishing (no token in
CI), and first-time packages fall back to a token automatically — because Trusted Publishing
cannot create a package that doesn't exist yet. You never hand-wire which packages use which
credential.

```yaml
npms:
  - scope: "@my-org"
    metapackage: my-cli
    bin: my-cli
    mode: optional-deps      # one metapackage, per-platform optionalDependencies
    access: public
    provenance: true         # npm provenance attestation
    auth: auto               # OIDC where possible, token where required
```

## Dual-license aware, per publisher

Rust crates are conventionally dual-licensed `MIT OR Apache-2.0`. anodizer parses the SPDX
expression once from `Cargo.toml` and renders it in each publisher's native shape — no per-channel
license strings to maintain.

```text
Cargo.toml:  license = "MIT OR Apache-2.0"

Homebrew cask →  license any_of: ["MIT", "Apache-2.0"]
AUR PKGBUILD  →  license=('MIT' 'Apache-2.0')
Nix package   →  license = with lib.licenses; [ mit asl20 ];
Chocolatey    →  SPDX-aware licenseUrl handling
```

A conjunctive expression (`A AND B`) renders as Homebrew `all_of:` and the matching list form for
each other channel. Single-license projects render the plain string. Nothing about your license is
duplicated by hand, so nothing drifts.

## macOS and Windows installers, built on Linux CI

anodizer assembles native installer formats reproducibly on an ordinary Linux runner — no macOS or
Windows host in the matrix. Code-signing and notarization still need the platform's own credentials,
but the *bundles themselves* are produced from Linux:

| Format | Built on Linux via |
|---|---|
| `.app` bundle | in-process directory + `Info.plist` assembly (no external tool) |
| `.dmg` | `genisoimage` / `mkisofs` |
| `.pkg` | flat XAR toolchain (`xar` + `mkbom`) |
| `.msi` | `wixl` (msitools) |
| `.exe` (NSIS) | `makensis` |

These installer formats are per-crate keys, so they live under a `crates[]` entry:

```yaml
crates:
  - name: my-cli
    app_bundles:
      - { name: My App, bundle: com.example.myapp }
    dmgs:
      - { name: "{{ .ProjectName }}-{{ .Version }}" }
    msis:
      - { version: v4 }      # WiX schema version (v3 or v4); auto-detected if omitted
    nsis:
      - { name: "{{ .ProjectName }}-installer" }
```

The installer bytes are covered by the [determinism harness](@/docs/advanced/determinism.md) like
every other artifact.

## Publisher resilience

Every publisher carries explicit rollback semantics. When a publisher fails, `on_error` hooks
fire after rollback has been attempted, with structured context — `ANODIZER_PUBLISHER`,
`ANODIZER_ERROR`, `ANODIZER_VERSION`, `ANODIZER_TAG`, `ANODIZER_GROUP`, `ANODIZER_REQUIRED`,
and `ANODIZER_ROLLED_BACK` env vars on the hook process, plus matching template variables.

```yaml
defaults:
  publish:
    on_error:
      - cmd: 'printf ''%s\n'' "$ANODIZER_PUBLISHER failed on $ANODIZER_TAG: $ANODIZER_ERROR"'

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
anodizer changelog --format release-notes           # GitHub release body to stdout
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

## Private and managed package registries

Beyond the public channels, anodizer uploads your Linux packages to managed registries in the same
run — `gemfury:` for Gem Fury accounts and `cloudsmiths:` for Cloudsmith repositories — with real
rollback support if a later required publisher fails.

```yaml
gemfury:
  - { account: my-org }                       # token from FURY_PUSH_TOKEN
cloudsmiths:
  - organization: my-org
    repository: stable
    distributions:
      deb: [ubuntu/jammy, debian/bookworm]
```

## Rust-ecosystem niceties

- **`cargo-binstall` metadata** derived from your build config automatically — `pkg-url` and
  per-target overrides come from your archive `name_template`, never hand-written.
- **Generated crate READMEs** kept in sync with a template.
- **AppImage packaging** for Linux — produces self-contained `.AppImage` bundles with optional
  zsync update metadata.
- **MCP publisher** — publishes a Model Context Protocol server manifest (pointing at your OCI
  image) to the [MCP registry](https://registry.modelcontextprotocol.io), with GitHub OIDC auth.
- **SchemaStore publisher** — submits your config schema to
  [SchemaStore.org](https://www.schemastore.org/) for IDE autocompletion.
- **Build from a branch** in CI via the GitHub Action — dogfood a release pipeline before tagging.
