# Configuration Reference

Anodize uses `.anodize.yaml` (or `.anodize.toml`) in your project root. Both formats use the same schema via serde.

## Full Example

```yaml
project_name: cfgd
dist: ./dist

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
      - name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        files:
          - LICENSE
          - README.md
      - name_template: "kubectl-{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        binaries: [kubectl-cfgd]
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
      prerelease: auto
      name_template: "{{ .Tag }}"
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
    tag_template: "cfgd-operator-v{{ .Version }}"
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
          - "ghcr.io/tj-smith47/cfgd-operator:{{ .Version }}"
          - "ghcr.io/tj-smith47/cfgd-operator:{{ .Tag }}"
        dockerfile: Dockerfile.operator.release
        platforms:
          - linux/amd64
          - linux/arm64
        binaries:
          - cfgd-operator

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

sign:
  artifacts: checksum  # none | all | checksum
  cmd: gpg
  args:
    - "--batch"
    - "--local-user"
    - "{{ .Env.GPG_FINGERPRINT }}"
    - "--output"
    - "{{ .Signature }}"
    - "--detach-sig"
    - "{{ .Artifact }}"

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

## Field Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `project_name` | string | **required** | Project name used in templates |
| `dist` | string | `./dist` | Output directory for artifacts |
| `defaults` | object | — | Workspace-level defaults inherited by crates |
| `before` | object | — | Hooks to run before the pipeline |
| `after` | object | — | Hooks to run after the pipeline |
| `crates` | array | **required** | Per-crate release configurations |
| `changelog` | object | — | Changelog generation settings |
| `sign` | object | — | Artifact signing config |
| `docker_signs` | array | — | Docker image signing configs |
| `snapshot` | object | — | Snapshot naming config |
| `announce` | object | — | Announcement provider configs |

### `defaults`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `targets` | string[] | — | Default Rust target triples |
| `cross` | string | `auto` | Cross-compilation strategy: `auto`, `zigbuild`, `cross`, `cargo` |
| `flags` | string | — | Default cargo build flags (e.g., `--release`) |
| `archives.format` | string | `tar.gz` | Default archive format |
| `archives.format_overrides` | array | — | OS-based format overrides (e.g., zip for windows) |
| `checksum.algorithm` | string | `sha256` | `sha256` or `sha512` |

### `crates[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Crate name (matches Cargo.toml) |
| `path` | string | **required** | Path to crate directory |
| `tag_template` | string | **required** | Git tag template (must contain `{{ .Version }}`) |
| `depends_on` | string[] | — | Crates that must be published first |
| `cross` | string | — | Per-crate cross-compilation strategy override |
| `builds` | array | — | Build configurations |
| `archives` | array or `false` | `[]` | Archive configurations, or `false` to disable |
| `checksum` | object | — | Per-crate checksum config override |
| `release` | object | — | GitHub Release config (omit to skip) |
| `publish` | object | — | Publishing config (crates.io, Homebrew, Scoop) |
| `docker` | array | — | Docker image configs |
| `nfpm` | array | — | Linux package configs |

### `crates[].builds[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `binary` | string | **required** | Binary name |
| `targets` | string[] | inherits defaults | Target triples for this binary |
| `features` | string[] | — | Cargo features to enable |
| `no_default_features` | bool | `false` | Disable default features |
| `env` | map | — | Per-target env vars (e.g., CC, AR) |
| `copy_from` | string | — | Copy another binary instead of building |
| `flags` | string | inherits defaults | Cargo build flags |

### `crates[].release`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `github.owner` | string | **required** | GitHub repo owner |
| `github.name` | string | **required** | GitHub repo name |
| `draft` | bool | `false` | Create as draft release |
| `prerelease` | `auto` or bool | `false` | `auto` detects from tag (-rc, -beta, -alpha) |
| `name_template` | string | `{{ .Tag }}` | Release name template |

### `crates[].publish`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `crates` | bool or object | `false` | Publish to crates.io |
| `crates.enabled` | bool | — | Enable crates.io publishing (object form) |
| `crates.index_timeout` | int | `300` | Seconds to wait for index (object form) |
| `homebrew.tap.owner` | string | — | Homebrew tap repo owner |
| `homebrew.tap.name` | string | — | Homebrew tap repo name |
| `homebrew.folder` | string | `Formula` | Formula directory in tap |
| `scoop.bucket.owner` | string | — | Scoop bucket repo owner |
| `scoop.bucket.name` | string | — | Scoop bucket repo name |

### `crates[].nfpm[]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `package_name` | string | crate name | Package name in the generated package |
| `formats` | string[] | **required** | Package formats: `deb`, `rpm`, `apk` |
| `vendor` | string | — | Package vendor |
| `homepage` | string | — | Project homepage URL |
| `maintainer` | string | — | Package maintainer email |
| `description` | string | — | Package description |
| `license` | string | — | Package license |
| `bindir` | string | `/usr/bin` | Binary installation directory |
| `file_name_template` | string | — | Output filename template |
| `contents` | array | — | Additional files to include (`src`, `dst` pairs) |
| `dependencies` | map | — | Per-format package dependencies (e.g., `deb: [libc6]`) |
| `overrides` | map | — | Per-format config overrides |

### Template Variables

| Variable | Description |
|----------|-------------|
| `{{ .ProjectName }}` | Project name from config |
| `{{ .Version }}` | Semantic version (without `v` prefix) |
| `{{ .Tag }}` | Full git tag |
| `{{ .ShortCommit }}` | Short commit hash |
| `{{ .FullCommit }}` | Full commit hash |
| `{{ .Os }}` | Mapped OS (linux, darwin, windows) |
| `{{ .Arch }}` | Mapped arch (amd64, arm64) |
| `{{ .Major }}` | Major version component |
| `{{ .Minor }}` | Minor version component |
| `{{ .Patch }}` | Patch version component |
| `{{ .Prerelease }}` | Prerelease suffix |
| `{{ .Date }}` | Current date |
| `{{ .Timestamp }}` | Unix timestamp |
| `{{ .IsSnapshot }}` | Whether snapshot mode |
| `{{ .IsDraft }}` | Whether release is a draft |
| `{{ .ReleaseURL }}` | URL of created GitHub release |
| `{{ .Env.VAR }}` | Environment variable |
| `{{ .Signature }}` | Signature output path (sign stage) |
| `{{ .Artifact }}` | Artifact input path (sign stage) |

### Target Triple Mapping

| Rust Target | `{{ .Os }}` | `{{ .Arch }}` |
|------------|-------------|---------------|
| `x86_64-unknown-linux-gnu` | `linux` | `amd64` |
| `aarch64-unknown-linux-gnu` | `linux` | `arm64` |
| `x86_64-apple-darwin` | `darwin` | `amd64` |
| `aarch64-apple-darwin` | `darwin` | `arm64` |
| `x86_64-pc-windows-msvc` | `windows` | `amd64` |
| `aarch64-pc-windows-msvc` | `windows` | `arm64` |
