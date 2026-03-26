# Anodize — Design Spec

**Date:** 2026-03-25
**Status:** Draft
**Author:** TJ Smith + Claude

## Overview

Anodize is a Rust-native, open-source alternative to GoReleaser. A single CLI binary that reads a declarative config file and executes a full release pipeline: build, archive, checksum, changelog, GitHub release, Docker images, package manager publishing, and more. Written in Rust for performance. Mirrors GoReleaser's UX and feature surface, adapted for Rust idioms and toolchain. GoReleaser Pro features are free.

Config filenames: `anodize.yaml` / `anodize.toml` (or `.anodize.yaml` / `.anodize.toml`).

### Goals

- Feature parity with GoReleaser OSS in Release 1
- GoReleaser Pro features (free) in Release 2
- First-class Rust workspace support with per-crate release cadences
- First-class crates.io publishing with dependency-aware ordering
- Familiar UX for GoReleaser users — same config structure, same CLI verbs, same template vocabulary
- Single statically-linked binary, installable via `cargo install anodize`

### Non-Goals (Release 1)

- Nightly builds
- Config includes/templates
- Split/merge (fan out builds in CI, merge artifacts)
- dmg, msi, pkg packaging
- Chocolatey, Winget
- Snapcraft
- Reproducible builds
- macOS Universal Binaries
- SBOM generation (e.g., syft integration)
- Source archives
- AUR (Arch User Repository) publishing
- UPX binary compression
- GitLab/Gitea support (Release 1 is GitHub-only; the `release` config is designed to be extensible to other forges)
- Announce providers beyond Discord, Slack, and generic HTTP webhooks (e.g., Telegram, Teams, Mastodon)

These are deferred to Release 2.

---

## Architecture

### Approach: Core + Stage Crates (Workspace)

A Cargo workspace where the core pipeline engine is one crate and each pipeline stage is its own crate implementing a shared trait. All stages compile into a single binary. Clean internal boundaries without user-facing complexity.

### Pipeline

```
Config Parse → Before Hooks → Build → Archive → NFpm → Checksum → Changelog
→ Release → Publish → Docker → Sign → Announce → After Hooks
```

### Core Trait

```rust
trait Stage {
    fn name(&self) -> &str;
    fn run(&self, ctx: &mut Context) -> Result<()>;
}
```

`&self` is intentional — all mutable state lives in `Context`, keeping stages stateless and composable. `Result` uses `anyhow::Result` for ergonomic error propagation; rich diagnostic formatting (colors, suggestions) is handled at the CLI layer, not in stages.

### Context

Shared mutable state flowing through the pipeline:

- **Config** — deserialized config file
- **Artifacts** — registry of everything produced (binaries, archives, checksums, Docker images). Each artifact has a type, path, target triple, and metadata. Downstream stages query by type.
- **Template variables** — `{{ .Version }}`, `{{ .Tag }}`, `{{ .ProjectName }}`, `{{ .Env.FOO }}`, target-specific `{{ .Os }}`, `{{ .Arch }}`, etc. Same vocabulary as GoReleaser.
- **Runtime info** — git state (tag, commit, branch, dirty), timestamp, semver components (`{{ .Major }}`, `{{ .Minor }}`, `{{ .Patch }}`, `{{ .Prerelease }}`)

### Workspace Layout

```
crates/
  core/            # Context, Stage trait, template engine, config schema, artifact registry
  stage-build/     # Cargo build orchestration, cross-compilation
  stage-archive/   # tar.gz, zip packaging
  stage-checksum/  # SHA256/SHA512 generation
  stage-changelog/ # Git log → grouped changelog
  stage-release/   # GitHub Release API
  stage-nfpm/      # .deb, .rpm, .apk Linux packages
  stage-publish/   # crates.io, Homebrew, Scoop
  stage-docker/    # Docker build/push
  stage-sign/      # GPG, cosign
  stage-announce/  # Webhooks (Discord, Slack, etc.)
  cli/             # Clap-based CLI, config loading, pipeline assembly
```

---

## Configuration

Supports both TOML and YAML, auto-detected by file extension (`anodize.yaml` or `anodize.toml`). YAML is the canonical documentation format; TOML support uses the same struct definitions via serde, so the schema is structurally identical.

### Full Config Schema

```yaml
project_name: cfgd
dist: ./dist  # Output directory for all artifacts (default: ./dist)

# Workspace-level defaults — crates inherit unless they override
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
    - aarch64-pc-windows-msvc
  cross: auto  # auto | zigbuild | cross | cargo
  flags: --release
  archives:
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
  checksum:
    algorithm: sha256

before:
  hooks:
    - cargo fmt --check
    - cargo clippy -- -D warnings

after:
  hooks:
    - echo "Release complete"

# Per-crate release configuration
# Version is read from each crate's Cargo.toml — not duplicated here
crates:
  - name: cfgd-core
    path: crates/cfgd-core
    tag_template: "cfgd-core-v{{ .Version }}"
    publish:
      crates: true

  - name: cfgd
    path: crates/cfgd
    tag_template: "v{{ .Version }}"
    depends_on:
      - cfgd-core
    builds:
      - binary: cfgd
        # Inherits targets from defaults
        features: []
        no_default_features: false
        env:
          aarch64-unknown-linux-gnu:
            CC: aarch64-linux-gnu-gcc
            AR: aarch64-linux-gnu-ar
      - binary: kubectl-cfgd
        # Copies the compiled cfgd binary under a new name during the build
        # stage, after compilation completes. Filesystem copy, not symlink.
        # Must reference a binary defined in the same crate's builds array.
        copy_from: cfgd
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    # When `binaries` is omitted from an archive config, all binaries
    # produced by this crate's builds are included.
    archives:
      - name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        files:
          - LICENSE
          - README.md
      - name_template: "kubectl-{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        binaries: [kubectl-cfgd]  # Only include kubectl-cfgd in this archive
        files:
          - LICENSE
    checksum:
      name_template: "{{ .ProjectName }}-{{ .Version }}-checksums.txt"
      algorithm: sha256
    release:
      github:
        owner: tj-smith47
        name: cfgd
      draft: false
      prerelease: auto  # true if tag contains -rc, -beta, etc.
      name_template: "{{ .Tag }}"
    publish:
      crates: true
      # Object form with options:
      # crates:
      #   enabled: true
      #   index_timeout: 300  # seconds to wait for crates.io indexing
      homebrew:
        tap:
          owner: tj-smith47
          name: homebrew-tap
        folder: Formula
        description: "Declarative, GitOps-style machine configuration management"
        license: Apache-2.0
        install: |
          bin.install "cfgd"
        test: |
          system "#{bin}/cfgd", "--version"
      scoop:
        bucket:
          owner: tj-smith47
          name: scoop-bucket
        description: "Declarative machine configuration management"

  - name: cfgd-operator
    path: crates/cfgd-operator
    tag_template: "cfgd-operator-v{{ .Version }}"
    builds:
      - binary: cfgd-operator
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
    # Disables archiving for this crate. Accepts false or an array of configs.
    archives: false
    # No release section — crates with no archives and no release block skip
    # the GitHub Release stage entirely. Their deliverable is a Docker image.
    publish:
      crates: false
    docker:
      - image_templates:
          - "ghcr.io/tj-smith47/cfgd-operator:{{ .Version }}"
          - "ghcr.io/tj-smith47/cfgd-operator:{{ .Tag }}"
        dockerfile: Dockerfile.operator.release
        platforms:
          - linux/amd64
          - linux/arm64
        binaries:
          - cfgd-operator

  - name: cfgd-csi
    path: crates/cfgd-csi
    tag_template: "cfgd-csi-v{{ .Version }}"
    builds:
      - binary: cfgd-csi
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
    archives: false
    publish:
      crates: false
    docker:
      - image_templates:
          - "ghcr.io/tj-smith47/cfgd-csi:{{ .Version }}"
          - "ghcr.io/tj-smith47/cfgd-csi:{{ .Tag }}"
        dockerfile: Dockerfile.csi.release
        platforms:
          - linux/amd64
          - linux/arm64
        binaries:
          - cfgd-csi

# nFPM-style Linux package generation (.deb, .rpm, .apk)
# Configured per-crate; omit to skip Linux packaging for that crate.
# nfpm:
#   - package_name: cfgd
#     formats:
#       - deb
#       - rpm
#       - apk
#     vendor: "TJ Smith"
#     homepage: "https://github.com/tj-smith47/cfgd"
#     maintainer: "TJ Smith <tj@example.com>"
#     description: "Declarative machine configuration management"
#     license: Apache-2.0
#     bindir: /usr/bin
#     contents:
#       - src: LICENSE
#         dst: /usr/share/doc/cfgd/LICENSE
#     dependencies:
#       deb:
#         - libc6
#     overrides:
#       rpm:
#         file_name_template: "{{ .ProjectName }}-{{ .Version }}.{{ .Arch }}"

# Changelog config is workspace-wide (filters, groups, sort order), but at
# runtime each crate's changelog is scoped to commits touching that crate's path.
changelog:
  sort: asc
  filters:
    exclude:
      - "^docs:"
      - "^ci:"
      - "^chore:"
  groups:
    - title: Features
      regexp: "^feat"
    - title: Bug Fixes
      regexp: "^fix"
    - title: Others
      order: 999

# signs and docker_signs are workspace-wide — they apply to all artifacts/images
# across all crates. Per-crate overrides are a Release 2 consideration.
# Accepts both `sign:` (single object, backward compat) and `signs:` (array).
signs:
  - id: default
    artifacts: checksum  # none | all | checksum | source | archive | binary | package
    cmd: gpg
    args:
      - "--batch"
      - "--local-user"
      - "{{ .Env.GPG_FINGERPRINT }}"
      - "--output"
      - "{{ .Signature }}"
      - "--detach-sig"
      - "{{ .Artifact }}"
    # signature: "{{ .Artifact }}.asc"  # optional: custom signature output path template
    # stdin: "passphrase"               # optional: pipe string to stdin
    # stdin_file: "/path/to/file"       # optional: pipe file contents to stdin
    # ids:                              # optional: filter by artifact IDs
    #   - my-archive

docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - "sign"
      - "{{ .Artifact }}"
      - "--yes"

snapshot:
  name_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"

announce:
  discord:
    enabled: false
    webhook_url: "{{ .Env.DISCORD_WEBHOOK_URL }}"
    message_template: "{{ .ProjectName }} {{ .Tag }} released! {{ .ReleaseURL }}"
  slack:
    enabled: false
    webhook_url: "{{ .Env.SLACK_WEBHOOK_URL }}"
    message_template: "{{ .ProjectName }} {{ .Tag }} released! {{ .ReleaseURL }}"
  webhook:
    enabled: false
    endpoint_url: "{{ .Env.WEBHOOK_URL }}"
    headers:
      Authorization: "Bearer {{ .Env.WEBHOOK_TOKEN }}"
    content_type: application/json
    message_template: '{"project":"{{ .ProjectName }}","tag":"{{ .Tag }}","url":"{{ .ReleaseURL }}"}'
```

---

## Cross-Compilation

Rust lacks Go's built-in cross-compilation. Anodize provides a transparent, layered strategy:

1. **Prefer `cargo-zigbuild`** — uses Zig as a cross-linker. No Docker, fast, handles most targets. Closest to Go's "just works" experience.
2. **Fall back to `cross`** — Docker-based. Seamless but requires Docker.
3. **Fall back to native `cargo build --target`** — requires user to install linkers/sysroots manually.

The `cross` config field controls this per-crate or in `defaults`:
- `auto` (default) — detect best available tool
- `zigbuild` — require cargo-zigbuild
- `cross` — require cross
- `cargo` — use native cargo build

Per-target environment variables (linkers, CC, AR) are set via the `env` map in build config.

---

## Template Engine

GoReleaser uses Go's `text/template` with `{{ .FieldName }}` dot-accessor syntax.

### Approach

Anodize uses the **[Tera](https://keats.github.io/tera/) template engine** with a Go-style preprocessor for migration compatibility. The preprocessor translates Go-style `{{ .Field }}` → `{{ Field }}` before passing templates to Tera, so users can copy template strings from GoReleaser configs without changes. Tera provides:

- **Variable substitution:** `{{ ProjectName }}`, `{{ Version }}`, `{{ Tag }}`, `{{ ShortCommit }}`, `{{ FullCommit }}`, `{{ Commit }}`
- **Nested access:** `{{ Env.VAR }}` — environment variable access
- **Target-specific:** `{{ Os }}`, `{{ Arch }}` — mapped from Rust target triples
- **Semver components:** `{{ Major }}`, `{{ Minor }}`, `{{ Patch }}`, `{{ Prerelease }}`
- **Temporal:** `{{ Date }}`, `{{ Timestamp }}`, `{{ CommitDate }}`, `{{ CommitTimestamp }}`, `{{ Now }}`
- **State:** `{{ IsSnapshot }}`, `{{ IsDraft }}`, `{{ IsGitDirty }}`, `{{ GitTreeState }}`
- **Git context:** `{{ Branch }}`, `{{ PreviousTag }}`, `{{ RawVersion }}`
- **Release:** `{{ ReleaseURL }}` — URL of the created GitHub release
- **Signing:** `{{ Signature }}`, `{{ Artifact }}` — used in sign stage config
- **Conditionals:** `{% if IsSnapshot %}...{% endif %}`
- **Loops:** `{% for item in list %}...{% endfor %}`
- **Pipe filters:** `{{ Tag | trimprefix(prefix="v") }}`, `{{ Name | toupper }}`

Custom GoReleaser-compatible filters: `tolower`, `toupper`, `trimprefix(prefix=...)`, `trimsuffix(suffix=...)`.

### Target Triple Mapping

Rust target triples are mapped to GoReleaser-compatible short names:

| Target Triple | `{{ .Os }}` | `{{ .Arch }}` |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `linux` | `amd64` |
| `aarch64-unknown-linux-gnu` | `linux` | `arm64` |
| `x86_64-apple-darwin` | `darwin` | `amd64` |
| `aarch64-apple-darwin` | `darwin` | `arm64` |
| `x86_64-pc-windows-msvc` | `windows` | `amd64` |
| `aarch64-pc-windows-msvc` | `windows` | `arm64` |

---

## CLI

Mirrors GoReleaser's command structure:

```
anodize release                          # Full pipeline (default)
anodize release --crate cfgd-core        # Release a specific crate
anodize release --crate cfgd-core --crate cfgd  # Multiple crates, dependency-ordered
anodize release --all                    # All crates with unreleased changes
anodize release --all --force            # All crates regardless of changes
anodize release --snapshot               # Build everything, don't publish
anodize release --skip=publish,announce  # Skip specific stages
anodize release --clean                  # Remove dist/ before building
anodize release --dry-run                # Full pipeline, skip all external side effects
anodize build                            # Build only
anodize build --crate cfgd              # Build a specific crate
anodize check                            # Validate config
anodize init                             # Generate starter config
anodize changelog                        # Generate changelog only
```

### `--skip` Stage Identifiers

Valid values for `--skip`: `build`, `archive`, `nfpm`, `checksum`, `changelog`, `release`, `publish`, `docker`, `sign`, `announce`.

### `--dry-run` vs `--snapshot`

- **`--snapshot`**: Builds with a snapshot version string (no tag required). Useful for testing builds locally. Does not publish.
- **`--dry-run`**: Runs the full pipeline with the real version/tag, validates everything, but skips all external side effects (GitHub release creation, Docker push, crates.io publish, Homebrew tap commit, announce webhooks). Useful for verifying a release pipeline end-to-end before committing to it.

### `init` Command

Rust-aware config generation:
- Reads `Cargo.toml` workspace members
- Discovers binary crates vs library crates
- Generates per-crate config entries with appropriate defaults
- Resolves workspace dependency graph for crates.io publish ordering
- Pre-populates target triples for common platforms

### Version and Tag Contract

Anodize expects a git tag matching the crate's `tag_template` (with `{{ .Version }}` replaced by the version from the crate's `Cargo.toml`) to exist at or before HEAD. Specifically:

- **Normal release:** Tag must exist at HEAD. If no matching tag is found, anodize exits with an error suggesting the user create the tag first (e.g., `git tag -a v0.2.0 -m "Release v0.2.0"`).
- **Snapshot mode (`--snapshot`):** No tag required. Version is read from `Cargo.toml` and the snapshot name template is applied.
- **Tag discovery:** For each crate, find the most recent tag matching the crate's `tag_template` pattern (with the version portion replaced by a semver regex). This is used to determine the previous version for changelog generation.

### Crate Selection Behavior

- `--crate cfgd` checks if `depends_on` crates need releasing first and prompts
- `--all` detects unreleased changes by comparing HEAD against each crate's latest tag, scoped to each crate's path
- Each crate gets its own changelog scoped to commits touching its path
- Each crate gets its own GitHub Release under its own tag (unless the crate has no `release` block, in which case the GitHub Release stage is skipped for that crate)

### Change Detection (`--all`)

When `--all` is used, anodize determines which crates have unreleased changes:

1. **Tag discovery:** For each crate, find the most recent tag matching the crate's `tag_template` pattern by scanning `git tag --list` and matching against the template with the version portion replaced by a semver regex (e.g., `tag_template: "cfgd-core-v{{ .Version }}"` → regex `^cfgd-core-v\d+\.\d+\.\d+`). Tags are sorted by semver to find the latest.
2. **No previous tag:** If no matching tag exists for a crate, it is treated as unreleased (all changes are new).
3. **Diff scope:** Compare `git diff --name-only <latest-tag>..HEAD` filtered to paths under the crate's `path` directory. Any file change within that path constitutes an unreleased change.
4. **Shared dependencies:** Changes to workspace-level files (`Cargo.toml`, `Cargo.lock`) are considered changes to all crates, since they may affect dependency resolution. This can be overridden with `--force` to release specific crates regardless.
5. **Ordering:** Crates with detected changes are topologically sorted by `depends_on` before execution.

### `check` Command — Config Validation

The `check` command validates the config at three levels:

1. **Schema validation** (errors): Field types are correct, required fields present, no unknown fields. Applies to both YAML and TOML.
2. **Semantic validation** (errors): Target triples are recognized, `depends_on` references exist and form a DAG (no cycles), referenced crate `path` directories exist, `tag_template` contains `{{ .Version }}`, `copy_from` references a binary defined in the same crate's `builds` array.
3. **Environment checks** (warnings): Cross-compilation tools available (`cargo-zigbuild`, `cross`), `docker` / `docker buildx` available if `docker` sections are configured, `gpg` / `cosign` available if `sign` sections are configured, `GITHUB_TOKEN` set if `release` sections are configured.

Errors cause a non-zero exit code. Warnings are printed but do not fail.

---

## Artifact System

Central to the pipeline, matching GoReleaser's internal model:

- Every stage that produces output registers it as an artifact with:
  - Type: `Binary`, `Archive`, `Checksum`, `DockerImage`, `LinuxPackage`, `Metadata`
  - Path on disk
  - Target triple
  - Crate name
  - Arbitrary metadata
- Downstream stages query artifacts by type and crate
- All artifacts written to `dist/` by default (configurable via `dist` top-level field)
- `archives` field accepts either `false` (disables archiving for that crate) or an array of archive configurations

---

## Stage Details

### Build (`stage-build`)

- Invokes cargo build (or zigbuild/cross) for each target in each crate's config
- Handles per-target env vars (CC, AR, linkers)
- Supports `copy_from` for binary aliasing — after compilation completes, the tool performs a filesystem copy of the compiled binary under the new name. Not a symlink. The `copy_from` value must reference a binary defined in the same crate's `builds` array.
- Registers `Binary` artifacts

### Archive (`stage-archive`)

- Packages binaries into tar.gz (unix) or zip (windows)
- When `binaries` is omitted from an archive config, all binaries produced by the crate's builds are included
- OS-specific format overrides via `format_overrides` (keyed by `os`, not Go's `goos`)
- Includes additional files (LICENSE, README, etc.)
- Name templating with all template variables
- Registers `Archive` artifacts
- Skipped for crates with `archives: false`

### NFpm (`stage-nfpm`)

- Generates `.deb`, `.rpm`, and `.apk` Linux packages from compiled binaries
- Shells out to `nfpm` CLI (must be installed; `check` command warns if missing)
- Per-crate config via `nfpm` block — package name, formats, metadata, contents, dependencies
- Per-format overrides for format-specific settings (e.g., different file name templates for RPM)
- Name templating with all template variables
- Registers `LinuxPackage` artifacts (uploaded as GitHub Release assets alongside archives)
- Skipped for crates with no `nfpm` block

### Checksum (`stage-checksum`)

- SHA256 (default) or SHA512 of all archive and Linux package artifacts
- Combined checksums file with configurable name template
- Per-archive `.sha256` files
- Registers `Checksum` artifacts
- Skipped for crates with `archives: false` (no archives to checksum)

### Changelog (`stage-changelog`)

- Built-in, matching GoReleaser's format
- Parses git log between previous tag and current tag
- Conventional commit grouping (feat, fix, etc.)
- Configurable sort order (asc/desc)
- Exclude filters by regex
- Custom groups with regex matching and ordering
- Per-crate scoping: only includes commits touching the crate's path (uses the workspace-wide `changelog` config for filters/groups/sort)

### Release (`stage-release`)

- GitHub Release creation via `octocrab`
- Uploads all `Archive` and `Checksum` artifacts as release assets
- Attaches changelog as release body
- Draft/prerelease support (`prerelease: auto` detects from tag)
- Token via `GITHUB_TOKEN` env var or `--token` flag
- Skipped for crates with no `release` block (e.g., Docker-only crates)

### Publish (`stage-publish`)

**crates.io:**
- `cargo publish` for each crate with `publish.crates` enabled
- `publish.crates` accepts either a boolean (`true`/`false`) or an object for additional options:
  ```yaml
  publish:
    crates:
      enabled: true
      index_timeout: 300  # seconds to wait for crates.io indexing (default: 300)
  ```
- Dependency-aware ordering via `depends_on`
- After publishing a dependency, polls the crates.io API to confirm the new version is indexed before publishing dependents. Retries up to 10 times with exponential backoff (starting at 5s, capped at 60s). Configurable via `publish.crates.index_timeout` (default: 5m).
- Token via `CARGO_REGISTRY_TOKEN` env var

**Homebrew:**
- Generates formula from config template
- Clones tap repo, writes formula, commits, pushes
- Multi-arch support with OS/arch detection in formula
- Token via `HOMEBREW_TAP_TOKEN` or `GITHUB_TOKEN`

**Scoop:**
- Generates manifest JSON
- Pushes to bucket repo
- Same auth as Homebrew

### Docker (`stage-docker`)

- Shells out to `docker buildx` for multi-arch builds
- Configurable Dockerfile, platforms, build args, tags
- Multiple image definitions per crate
- Login handled externally (user runs `docker login` or CI provides creds)
- Registers `DockerImage` artifacts

**Multi-arch binary staging:** Anodize stages pre-built binaries into architecture-specific directories before invoking `docker buildx`. The Docker build context is set to the staging directory:

```
dist/docker/<crate>/<image-index>/    <- build context
  binaries/
    amd64/
      <binary>
    arm64/
      <binary>
  Dockerfile                          <- copied from the configured dockerfile path
```

Anodize copies the configured Dockerfile into the staging directory alongside the binaries, then invokes `docker buildx build` with the staging directory as the build context. The Dockerfile uses `TARGETARCH` (provided automatically by `docker buildx`) to select the correct binary:

```dockerfile
FROM debian:bookworm-slim
ARG TARGETARCH
COPY binaries/${TARGETARCH}/<binary> /usr/local/bin/<binary>
```

Invocation:
```
docker buildx build \
  --platform=linux/amd64,linux/arm64 \
  --push \
  --tag <image>:<tag> \
  dist/docker/<crate>/<image-index>/
```

This matches the pattern used in cfgd's existing release Dockerfiles.

### Sign (`stage-sign`)

- GPG signing of checksum files (shells out to `gpg`)
- Cosign signing of Docker images (shells out to `cosign`)
- Configurable via `signs` (array) and `docker_signs` sections; backward compat: `sign` (single object) is auto-wrapped into an array
- Multiple sign configs supported, each with optional `id`, `artifacts` filter (`none`, `all`, `checksum`, `source`, `archive`, `binary`, `package`), `ids` filter, `signature` template, `stdin`/`stdin_file` for piping to the signing command
- Template variables `{{ .Signature }}` and `{{ .Artifact }}` resolve to output path and input artifact path respectively
- Same config surface as GoReleaser

### Announce (`stage-announce`)

- Discord webhook
- Slack webhook
- Generic HTTP webhook (custom endpoint, headers, content type)
- Message templating with all template variables
- Each provider independently enabled/disabled
- Additional providers (Telegram, Teams, Mastodon, etc.) deferred to Release 2

---

## Error Handling & UX

- Colored terminal output with stage-by-stage progress (similar to GoReleaser's output)
- `--verbose` / `--debug` flags for detailed logging
- Fail-fast by default
- Clear error messages with actionable suggestions (e.g., "cargo-zigbuild not found — install with `cargo install cargo-zigbuild` or set `cross: cargo` with appropriate linkers")
- `check` command validates config before running (see `check` command section for validation levels)

---

## GitHub API Integration

- `octocrab` crate for GitHub API
- Create releases, upload assets, update Homebrew/Scoop repos
- Token via `GITHUB_TOKEN` env var or `--token` flag
- Same auth model as GoReleaser

---

## Workspace vs Monorepo Distinction

**Release 1 — Workspace support:** A single Cargo workspace (one root `Cargo.toml` with `[workspace]`) containing multiple crates that may release independently on their own cadences. One config file governs all crates. This is standard Rust project structure.

**Release 2 — Monorepo support:** Multiple independent Cargo workspaces (separate `Cargo.toml` root files) or mixed-language projects coexisting in a single Git repository, each with its own config file. A `--project` flag or directory-based config discovery selects which project to release. Example: a repo containing `rust-services/Cargo.toml` and `go-tools/` side by side.

---

## Release 2: Pro Features (Free)

Deferred features, all shipping as free in the second release:

- **Monorepo support** — multiple independent workspaces or mixed-language projects in one repo (see distinction above)
- **Nightly builds** — `--nightly` flag, scheduled builds with `{{ .Nightly }}` template var
- **Config includes/templates** — split config across files, `includes:` directive
- **Split/merge** — fan out builds across CI runners, merge artifacts into one release
- **Snapcraft** — Linux snap packaging
- **dmg, msi, pkg packaging** — native OS installers
- **Chocolatey, Winget** — Windows package managers
- **Reproducible builds** — deterministic output with `SOURCE_DATE_EPOCH`
- **macOS Universal Binaries** — fat binaries combining x86_64 + aarch64
