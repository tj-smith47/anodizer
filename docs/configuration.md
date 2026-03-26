# Configuration Reference

Anodize uses `.anodize.yaml` (or `.anodize.toml`) in your project root. Both formats use the same schema via serde.

## CLI Flags

### Global Flags

| Flag | Short | Applies to | Default | Description |
|------|-------|-----------|---------|-------------|
| `--config` | `-f` | All commands | `.anodize.yaml` | Path to config file (overrides auto-detection) |
| `--verbose` | -- | All commands | `false` | Enable verbose output |
| `--debug` | -- | All commands | `false` | Enable debug output |

### `release` Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--crate <name>` | -- | -- | Release a specific crate (repeatable) |
| `--all` | -- | `false` | Release all crates with unreleased changes |
| `--force` | -- | `false` | Force release even without unreleased changes |
| `--snapshot` | -- | `false` | Build without publishing (snapshot mode) |
| `--dry-run` | -- | `false` | Full pipeline with no side effects |
| `--clean` | -- | `false` | Remove `dist/` directory before starting |
| `--skip <stages>` | -- | -- | Skip stages (comma-separated, e.g. `--skip=docker,announce`) |
| `--token <token>` | -- | -- | GitHub token (overrides `GITHUB_TOKEN` env var) |
| `--timeout` | -- | `30m` | Pipeline timeout duration (e.g. `30m`, `1h`, `5s`) |
| `--parallelism` | `-p` | CPU count | Maximum number of parallel build jobs |
| `--auto-snapshot` | -- | `false` | Automatically enable `--snapshot` if the git repo is dirty |
| `--single-target` | -- | `false` | Build only for the host target triple |
| `--release-notes <path>` | -- | -- | Path to a custom release notes file (overrides changelog) |

### `build` Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--crate <name>` | -- | -- | Build a specific crate (repeatable) |
| `--timeout` | -- | `30m` | Pipeline timeout duration |
| `--parallelism` | `-p` | CPU count | Maximum number of parallel build jobs |
| `--single-target` | -- | `false` | Build only for the host target triple |

### `changelog` Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--crate <name>` | -- | -- | Generate changelog for a specific crate |

### Other Commands

| Command | Description |
|---------|-------------|
| `anodize check` | Validate configuration file |
| `anodize init` | Generate a starter config from your Cargo workspace |
| `anodize changelog` | Generate changelog only |
| `anodize completion <shell>` | Generate shell completions (`bash`, `zsh`, `fish`, `powershell`) |
| `anodize healthcheck` | Check availability of required external tools |

## Auto-detection

When `release.github.owner` and `release.github.name` are omitted from the config, anodize will attempt to auto-detect them from the git remote URL (supports both HTTPS and SSH remote formats). You can still set them explicitly to override auto-detection.

## Full Example

```yaml
project_name: cfgd
dist: ./dist

env:
  SOME_GLOBAL_VAR: "value"

report_sizes: true

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

crates:
  - name: cfgd-core
    path: crates/cfgd-core
    tag_template: "cfgd-core-v{{ Version }}"
    publish:
      crates: true

  - name: cfgd
    path: crates/cfgd
    tag_template: "v{{ Version }}"
    depends_on:
      - cfgd-core
    builds:
      - binary: cfgd
        features: []
        no_default_features: false
        env:
          aarch64-unknown-linux-gnu:
            CC: aarch64-linux-gnu-gcc
            AR: aarch64-linux-gnu-ar
      - binary: kubectl-cfgd
        copy_from: cfgd
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
          - x86_64-apple-darwin
          - aarch64-apple-darwin
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
        format: tar.gz
        wrap_in_directory: "{{ ProjectName }}-{{ Version }}"
        files:
          - LICENSE
          - README.md
          - "docs/*.md"  # glob patterns supported
      - name_template: "kubectl-{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
        binaries: [kubectl-cfgd]
        files:
          - LICENSE
    checksum:
      name_template: "{{ ProjectName }}-{{ Version }}-checksums.txt"
      algorithm: sha256
      extra_files:
        - "./extra-artifact.tar.gz"
      ids:
        - cfgd
    release:
      github:
        owner: tj-smith47
        name: cfgd
      draft: false
      prerelease: auto
      make_latest: auto
      name_template: "{{ Tag }}"
      header: "## What's New in {{ Tag }}"
      footer: "Full changelog: https://github.com/tj-smith47/cfgd/compare/{{ PreviousTag }}...{{ Tag }}"
      extra_files:
        - "./installer.sh"
      skip_upload: false
      replace_existing_draft: true
      replace_existing_artifacts: true
    publish:
      crates: true
      # Object form with options:
      # crates:
      #   enabled: true
      #   index_timeout: 300
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
    tag_template: "cfgd-operator-v{{ Version }}"
    builds:
      - binary: cfgd-operator
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
    archives: false
    publish:
      crates: false
    docker:
      - image_templates:
          - "ghcr.io/tj-smith47/cfgd-operator:{{ Version }}"
          - "ghcr.io/tj-smith47/cfgd-operator:{{ Tag }}"
        dockerfile: Dockerfile.operator.release
        platforms:
          - linux/amd64
          - linux/arm64
        binaries:
          - cfgd-operator
        skip_push: false
        extra_files:
          - config.yaml
        push_flags:
          - "--quiet"
    nfpm:
      - package_name: cfgd-operator
        formats:
          - deb
          - rpm
        vendor: "TJ Smith"
        homepage: "https://github.com/tj-smith47/cfgd"
        maintainer: "TJ Smith <tj@example.com>"
        description: "Kubernetes operator for cfgd"
        license: Apache-2.0
        bindir: /usr/bin
        file_name_template: "{{ PackageName }}_{{ Version }}_{{ Arch }}"
        scripts:
          postinstall: scripts/postinstall.sh
          preremove: scripts/preremove.sh
        dependencies:
          deb:
            - libc6
          rpm:
            - glibc
        recommends:
          - kubectl
        suggests:
          - helm
        conflicts:
          - cfgd-operator-legacy
        replaces:
          - cfgd-operator-legacy
        provides:
          - cfgd-operator
        contents:
          - src: config/defaults.yaml
            dst: /etc/cfgd-operator/defaults.yaml
            type: config
            file_info:
              owner: root
              group: root
              mode: "0644"

changelog:
  use: git  # git | github-native
  header: "# Changelog"
  footer: ""
  sort: asc
  abbrev: 7
  filters:
    exclude:
      - "^docs:"
      - "^ci:"
      - "^chore:"
    include:
      - "^feat"
      - "^fix"
  groups:
    - title: Features
      regexp: "^feat"
    - title: Bug Fixes
      regexp: "^fix"
    - title: Others
      order: 999

# Singular `sign:` (object) or plural `signs:` (array) both work
signs:
  - id: gpg-sign
    artifacts: checksum  # none | all | checksum
    cmd: gpg
    args:
      - "--batch"
      - "--local-user"
      - "{{ Env.GPG_FINGERPRINT }}"
      - "--output"
      - "{{ Signature }}"
      - "--detach-sig"
      - "{{ Artifact }}"
    signature: "${artifact}.sig"
    ids:
      - cfgd
    # stdin: "passphrase"
    # stdin_file: /path/to/passphrase

docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - "sign"
      - "{{ Artifact }}"
      - "--yes"

publishers:
  - name: custom-publish
    cmd: ./scripts/publish.sh
    args:
      - "{{ ArtifactPath }}"
      - "{{ Version }}"
    ids:
      - cfgd
    artifact_types:
      - archive
      - checksum
    env:
      PUBLISH_TOKEN: "{{ Env.PUBLISH_TOKEN }}"

snapshot:
  name_template: "{{ Tag | trimprefix(prefix='v') }}-SNAPSHOT-{{ ShortCommit }}"

announce:
  discord:
    enabled: false
    webhook_url: "{{ Env.DISCORD_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} released! {{ ReleaseURL }}"
  slack:
    enabled: false
    webhook_url: "{{ Env.SLACK_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} released! {{ ReleaseURL }}"
  webhook:
    enabled: false
    endpoint_url: "{{ Env.WEBHOOK_URL }}"
    headers:
      Authorization: "Bearer {{ Env.WEBHOOK_TOKEN }}"
    content_type: application/json
    message_template: '{"project":"{{ ProjectName }}","tag":"{{ Tag }}","url":"{{ ReleaseURL }}"}'
```

## Field Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `project_name` | string | **required** | Project name used in templates |
| `dist` | string | `./dist` | Output directory for artifacts |
| `env` | map | -- | Global environment variables available in templates via `{{ Env.VAR }}` |
| `report_sizes` | bool | `false` | Print artifact sizes after each stage |
| `defaults` | object | -- | Workspace-level defaults inherited by crates |
| `before` | object | -- | Hooks to run before the pipeline |
| `after` | object | -- | Hooks to run after the pipeline |
| `crates` | array | **required** | Per-crate release configurations |
| `changelog` | object | -- | Changelog generation settings |
| `signs` | array | -- | Artifact signing configs (also accepts singular `sign:` as an object) |
| `docker_signs` | array | -- | Docker image signing configs |
| `snapshot` | object | -- | Snapshot naming config |
| `announce` | object | -- | Announcement provider configs |
| `publishers` | array | -- | Custom publisher commands for post-release artifact publishing |

### `defaults`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `targets` | string[] | -- | Default Rust target triples |
| `cross` | string | `auto` | Cross-compilation strategy: `auto`, `zigbuild`, `cross`, `cargo` |
| `flags` | string | -- | Default cargo build flags (e.g., `--release`) |
| `archives.format` | string | `tar.gz` | Default archive format |
| `archives.format_overrides` | array | -- | OS-based format overrides (e.g., zip for windows) |
| `checksum.algorithm` | string | `sha256` | Default hash algorithm |
| `checksum.name_template` | string | `{{ ProjectName }}-{{ Version }}-checksums.txt` | Default checksum filename template |
| `checksum.disable` | bool | `false` | Disable checksum generation globally |
| `checksum.extra_files` | string[] | -- | Additional files to include in checksums |
| `checksum.ids` | string[] | -- | Limit checksums to artifacts with these IDs |

### `before` / `after`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `hooks` | string[] | -- | Shell commands to run sequentially |

### `crates[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Crate name (matches Cargo.toml) |
| `path` | string | **required** | Path to crate directory |
| `tag_template` | string | **required** | Git tag template (must contain `{{ Version }}`) |
| `depends_on` | string[] | -- | Crates that must be published first |
| `cross` | string | -- | Per-crate cross-compilation strategy override |
| `builds` | array | -- | Build configurations |
| `archives` | array or `false` | `[]` | Archive configurations, or `false` to disable |
| `checksum` | object | -- | Per-crate checksum config override |
| `release` | object | -- | GitHub Release config (omit to skip) |
| `publish` | object | -- | Publishing config (crates.io, Homebrew, Scoop) |
| `docker` | array | -- | Docker image configs |
| `nfpm` | array | -- | Linux package configs |

### `crates[].builds[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `binary` | string | **required** | Binary name |
| `targets` | string[] | inherits defaults | Target triples for this binary |
| `features` | string[] | -- | Cargo features to enable |
| `no_default_features` | bool | `false` | Disable default features |
| `env` | map | -- | Per-target env vars (e.g., `CC`, `AR`). Keys are target triples. |
| `copy_from` | string | -- | Copy another binary instead of building (for renamed binaries) |
| `flags` | string | inherits defaults | Cargo build flags |

### `crates[].archives[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | -- | Archive filename template (without extension) |
| `format` | string | inherits defaults | Archive format: `tar.gz`, `tar.xz`, `tar.zst`, `zip`, `binary` |
| `format_overrides` | array | -- | OS-based format overrides |
| `files` | string[] | -- | Additional files to include (supports glob patterns like `docs/*.md`) |
| `binaries` | string[] | -- | Specific binaries to include (defaults to all) |
| `wrap_in_directory` | string | -- | Wrap archive contents in a directory (supports templates) |

Set `archives: false` to disable archive creation entirely (useful for Docker-only crates).

**Format overrides:**

```yaml
format_overrides:
  - os: windows
    format: zip
```

**The `binary` format** copies the raw binary without any archive compression. Useful for single-binary distributions.

### `crates[].checksum`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ ProjectName }}-{{ Version }}-checksums.txt` | Checksum filename template |
| `algorithm` | string | `sha256` | Hash algorithm (see below) |
| `disable` | bool | `false` | Disable checksum generation for this crate |
| `extra_files` | string[] | -- | Additional files to include in the checksum file |
| `ids` | string[] | -- | Limit checksums to artifacts with these IDs |

**Supported algorithms:** `sha1`, `sha224`, `sha256`, `sha384`, `sha512`, `blake2b`, `blake2s`

### `crates[].release`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `github.owner` | string | auto-detected | GitHub repo owner (auto-detected from git remote if omitted) |
| `github.name` | string | auto-detected | GitHub repo name (auto-detected from git remote if omitted) |
| `draft` | bool | `false` | Create as draft release |
| `prerelease` | `auto` or bool | `false` | `auto` detects from tag (e.g., `-rc`, `-beta`, `-alpha`) |
| `make_latest` | `auto`, `true`, or `false` | `auto` | Whether to mark release as "latest" on GitHub |
| `name_template` | string | `{{ Tag }}` | Release name template |
| `header` | string | -- | Text prepended to the release body (supports templates) |
| `footer` | string | -- | Text appended to the release body (supports templates) |
| `extra_files` | string[] | -- | Additional files to upload as release assets |
| `skip_upload` | bool | `false` | Skip uploading artifacts to the GitHub release |
| `replace_existing_draft` | bool | `false` | Replace an existing draft release with the same tag |
| `replace_existing_artifacts` | bool | `false` | Replace existing artifacts if they already exist on the release |

### `crates[].publish`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `crates` | bool or object | `false` | Publish to crates.io |
| `crates.enabled` | bool | -- | Enable crates.io publishing (object form) |
| `crates.index_timeout` | int | `300` | Seconds to wait for crates.io index propagation (object form) |
| `homebrew.tap.owner` | string | -- | Homebrew tap repo owner |
| `homebrew.tap.name` | string | -- | Homebrew tap repo name |
| `homebrew.folder` | string | `Formula` | Formula directory in tap |
| `homebrew.description` | string | -- | Formula description |
| `homebrew.license` | string | -- | Formula license |
| `homebrew.install` | string | -- | Homebrew install script |
| `homebrew.test` | string | -- | Homebrew test script |
| `scoop.bucket.owner` | string | -- | Scoop bucket repo owner |
| `scoop.bucket.name` | string | -- | Scoop bucket repo name |
| `scoop.description` | string | -- | Scoop manifest description |
| `scoop.license` | string | -- | Scoop manifest license |

### `crates[].docker[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image_templates` | string[] | **required** | Docker image tag templates |
| `dockerfile` | string | **required** | Path to Dockerfile |
| `platforms` | string[] | -- | Target platforms (e.g., `linux/amd64`, `linux/arm64`) |
| `binaries` | string[] | -- | Binaries to include in the Docker build context |
| `build_flag_templates` | string[] | -- | Additional `docker build` flags (supports templates) |
| `skip_push` | bool | `false` | Build images but skip pushing to registry |
| `extra_files` | string[] | -- | Additional files to copy into the Docker staging directory |
| `push_flags` | string[] | -- | Additional flags passed to `docker push` |

### `crates[].nfpm[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `package_name` | string | crate name | Package name in the generated package |
| `formats` | string[] | **required** | Package formats: `deb`, `rpm`, `apk` |
| `vendor` | string | -- | Package vendor |
| `homepage` | string | -- | Project homepage URL |
| `maintainer` | string | -- | Package maintainer email |
| `description` | string | -- | Package description |
| `license` | string | -- | Package license |
| `bindir` | string | `/usr/bin` | Binary installation directory |
| `file_name_template` | string | -- | Output filename template |
| `scripts` | object | -- | Package lifecycle scripts |
| `scripts.preinstall` | string | -- | Script run before installation |
| `scripts.postinstall` | string | -- | Script run after installation |
| `scripts.preremove` | string | -- | Script run before removal |
| `scripts.postremove` | string | -- | Script run after removal |
| `contents` | array | -- | Additional files to include in the package |
| `dependencies` | map | -- | Per-format package dependencies (e.g., `deb: [libc6]`) |
| `overrides` | map | -- | Per-format config overrides |
| `recommends` | string[] | -- | Recommended packages (deb/rpm) |
| `suggests` | string[] | -- | Suggested packages (deb) |
| `conflicts` | string[] | -- | Packages that conflict with this one |
| `replaces` | string[] | -- | Packages that this one replaces |
| `provides` | string[] | -- | Virtual packages that this one provides |

### `crates[].nfpm[].contents[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `src` | string | **required** | Source file path |
| `dst` | string | **required** | Destination path in the package |
| `type` | string | -- | Content type (e.g., `config` for configuration files) |
| `file_info` | object | -- | File ownership and permissions |
| `file_info.owner` | string | -- | File owner |
| `file_info.group` | string | -- | File group |
| `file_info.mode` | string | -- | File mode (e.g., `"0644"`) |

### `changelog`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `use` | string | `git` | Changelog source: `git` (from commit history) or `github-native` (GitHub's auto-generated notes) |
| `header` | string | -- | Header text prepended to the changelog (supports templates) |
| `footer` | string | -- | Footer text appended to the changelog (supports templates) |
| `disable` | bool | `false` | Disable changelog generation entirely |
| `sort` | string | `asc` | Sort order for commits: `asc` or `desc` |
| `abbrev` | int | `7` | Git commit hash abbreviation length |
| `filters.exclude` | string[] | -- | Regex patterns to exclude commits |
| `filters.include` | string[] | -- | Regex patterns to include commits (if set, only matching commits are included) |
| `groups` | array | -- | Commit groups with `title`, `regexp`, `order` |

When `use: github-native` is set, anodize delegates changelog generation to the GitHub API (`generate_release_notes`). The `filters`, `groups`, `sort`, and `abbrev` fields are ignored in this mode.

### `signs[]`

Artifact signing configuration. Accepts either a single object (`sign:`) or an array (`signs:`).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | -- | Unique identifier for this signing config |
| `artifacts` | string | `none` | Which artifacts to sign: `none`, `all`, `checksum` |
| `cmd` | string | -- | Signing command (e.g., `gpg`, `cosign`) |
| `args` | string[] | -- | Arguments to the signing command (supports templates) |
| `signature` | string | -- | Signature output filename pattern |
| `stdin` | string | -- | String to pipe to the signing command's stdin |
| `stdin_file` | string | -- | Path to a file whose contents are piped to stdin |
| `ids` | string[] | -- | Limit signing to artifacts with these IDs |

### `docker_signs[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `artifacts` | string | `none` | Which artifacts to sign: `none`, `all` |
| `cmd` | string | -- | Signing command (e.g., `cosign`) |
| `args` | string[] | -- | Arguments to the signing command (supports templates) |

### `publishers[]`

Custom publisher commands for generic post-release artifact publishing.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | -- | Human-readable name for this publisher |
| `cmd` | string | **required** | Command to execute |
| `args` | string[] | -- | Arguments to the command (supports templates) |
| `ids` | string[] | -- | Limit to artifacts with these IDs |
| `artifact_types` | string[] | -- | Limit to specific artifact types (e.g., `archive`, `checksum`) |
| `env` | map | -- | Additional environment variables for the command |

### `snapshot`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | -- | Version string template for snapshot builds |

### `announce`

#### `announce.discord` / `announce.slack`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable this announcement provider |
| `webhook_url` | string | -- | Webhook URL (supports templates for env vars) |
| `message_template` | string | -- | Message template |

#### `announce.webhook`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable webhook announcements |
| `endpoint_url` | string | -- | Webhook endpoint URL (supports templates) |
| `headers` | map | -- | HTTP headers (supports templates) |
| `content_type` | string | -- | Content-Type header value |
| `message_template` | string | -- | Request body template |

### Template Variables

See the [Template Reference](templates.md) for the complete list of template variables, filters, and Tera features.

### Target Triple Mapping

| Rust Target | `{{ Os }}` | `{{ Arch }}` |
|------------|-------------|---------------|
| `x86_64-unknown-linux-gnu` | `linux` | `amd64` |
| `aarch64-unknown-linux-gnu` | `linux` | `arm64` |
| `x86_64-apple-darwin` | `darwin` | `amd64` |
| `aarch64-apple-darwin` | `darwin` | `arm64` |
| `x86_64-pc-windows-msvc` | `windows` | `amd64` |
| `aarch64-pc-windows-msvc` | `windows` | `arm64` |
