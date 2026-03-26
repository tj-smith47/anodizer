# Migration from GoReleaser

This guide covers translating the 10 most common GoReleaser configuration patterns into Anodize equivalents. Anodize is designed to feel familiar to GoReleaser users while being native to the Rust ecosystem.

## Key Differences

Before diving into specific patterns, here are the fundamental differences:

| Aspect | GoReleaser | Anodize |
|--------|-----------|---------|
| Language | Go | Rust |
| Build system | `go build` | `cargo build` |
| Template engine | Go `text/template` | [Tera](https://keats.github.io/tera/) (Jinja2-like) |
| Template syntax | `{{ .Field }}` | `{{ Field }}` (both supported) |
| Workspace | Single project | Multi-crate workspaces with `crates[]` |
| Cross-compilation | `GOOS`/`GOARCH` | Target triples via `cargo-zigbuild`/`cross` |
| Config file | `.goreleaser.yaml` | `.anodize.yaml` |

## Template Syntax

Anodize accepts GoReleaser's `{{ .Field }}` syntax for migration convenience. The leading dot is automatically stripped. You can keep your existing templates or migrate them to native Tera syntax.

**GoReleaser:**
```yaml
name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"
```

**Anodize (both work):**
```yaml
# Go-style (works as-is from GoReleaser)
name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"

# Native Tera style
name_template: "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}"
```

**Template function mapping:**

| GoReleaser | Anodize | Notes |
|-----------|---------|-------|
| `{{ tolower .Os }}` | `{{ Os \| tolower }}` | Tera uses pipe syntax |
| `{{ toupper .Arch }}` | `{{ Arch \| toupper }}` | Same |
| `{{ trimprefix .Tag "v" }}` | `{{ Tag \| trimprefix(prefix="v") }}` | Named argument |
| `{{ trimsuffix .Name ".exe" }}` | `{{ Name \| trimsuffix(suffix=".exe") }}` | Named argument |
| `{{ if .IsSnapshot }}` | `{% if IsSnapshot %}` | Tera uses `{% %}` for blocks |
| `{{ end }}` | `{% endif %}` | Explicit end tags |
| `{{ .Env.VAR }}` | `{{ Env.VAR }}` | Same structure |

---

## Pattern 1: Basic Build Configuration

**GoReleaser:**
```yaml
project_name: myapp
builds:
  - env:
      - CGO_ENABLED=0
    goos:
      - linux
      - darwin
      - windows
    goarch:
      - amd64
      - arm64
    ldflags:
      - -s -w -X main.version={{.Version}}
```

**Anodize:**
```yaml
project_name: myapp
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
    - aarch64-pc-windows-msvc
  cross: auto
  flags: --release

crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
```

**Key differences:**
- GoReleaser uses `goos`/`goarch` pairs; Anodize uses full Rust target triples.
- `cross: auto` automatically selects the best cross-compilation tool (`cargo-zigbuild`, `cross`, or native `cargo`).
- Rust release builds use `--release` flag instead of Go's `ldflags`.

---

## Pattern 2: Archives with Format Overrides

**GoReleaser:**
```yaml
archives:
  - name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"
    format: tar.gz
    format_overrides:
      - goos: windows
        format: zip
    files:
      - LICENSE
      - README.md
      - docs/*
    wrap_in_directory: true
```

**Anodize:**
```yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
    archives:
      - name_template: "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}"
        format: tar.gz
        format_overrides:
          - os: windows
            format: zip
        files:
          - LICENSE
          - README.md
          - "docs/*"
        wrap_in_directory: "{{ ProjectName }}-{{ Version }}"
```

**Key differences:**
- Archives are nested under `crates[]` instead of being top-level.
- `goos` is renamed to `os` in format overrides.
- `wrap_in_directory` accepts a template string (not just `true`/`false`).
- Glob patterns in `files` are supported (e.g., `docs/*.md`).
- Additional formats supported: `tar.xz`, `tar.zst`, `binary` (raw binary, no compression).

---

## Pattern 3: Checksums

**GoReleaser:**
```yaml
checksum:
  name_template: "checksums.txt"
  algorithm: sha256
  extra_files:
    - glob: ./installer.sh
```

**Anodize:**
```yaml
crates:
  - name: myapp
    # ...
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
      extra_files:
        - "./installer.sh"
```

**Key differences:**
- Checksum config is per-crate, or set globally in `defaults.checksum`.
- Supported algorithms: `sha1`, `sha224`, `sha256`, `sha384`, `sha512`, `blake2b`, `blake2s`.
- `extra_files` is a flat string array (no `glob:` wrapper).
- Use `ids` to limit checksums to specific artifact IDs.

---

## Pattern 4: GitHub Release

**GoReleaser:**
```yaml
release:
  github:
    owner: myorg
    name: myapp
  draft: false
  prerelease: auto
  make_latest: true
  name_template: "{{ .Tag }}"
  header: |
    ## What's Changed
  footer: |
    **Full Changelog**: https://github.com/myorg/myapp/compare/{{ .PreviousTag }}...{{ .Tag }}
  extra_files:
    - glob: ./dist/installer.sh
  replace_existing_draft: true
  replace_existing_artifacts: true
```

**Anodize:**
```yaml
crates:
  - name: myapp
    # ...
    release:
      github:
        owner: myorg
        name: myapp
      draft: false
      prerelease: auto
      make_latest: true
      name_template: "{{ Tag }}"
      header: |
        ## What's Changed
      footer: |
        **Full Changelog**: https://github.com/myorg/myapp/compare/{{ PreviousTag }}...{{ Tag }}
      extra_files:
        - "./dist/installer.sh"
      replace_existing_draft: true
      replace_existing_artifacts: true
```

**Key differences:**
- Release config is per-crate.
- `github.owner` and `github.name` are auto-detected from the git remote if omitted.
- `skip_upload: true` skips artifact uploads to the release.
- `extra_files` is a flat string array.

---

## Pattern 5: Changelog

**GoReleaser:**
```yaml
changelog:
  sort: asc
  use: github-native
  abbrev: 0
  filters:
    exclude:
      - "^docs:"
      - "^test:"
  groups:
    - title: Features
      regexp: "^.*feat.*$"
      order: 0
    - title: Bug Fixes
      regexp: "^.*fix.*$"
      order: 1
    - title: Others
      order: 999
```

**Anodize:**
```yaml
changelog:
  sort: asc
  use: github-native
  abbrev: 7
  filters:
    exclude:
      - "^docs:"
      - "^test:"
    include:
      - "^feat"
      - "^fix"
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
    - title: Others
      order: 999
```

**Key differences:**
- `use: github-native` delegates changelog generation to GitHub's `generate_release_notes` API.
- When using `github-native`, filters/groups/sort/abbrev are ignored.
- `filters.include` is available to whitelist commits (GoReleaser uses `include` under filters too).
- `header` and `footer` fields support template variables.

---

## Pattern 6: Docker Images

**GoReleaser:**
```yaml
dockers:
  - image_templates:
      - "ghcr.io/myorg/myapp:{{ .Version }}"
      - "ghcr.io/myorg/myapp:latest"
    dockerfile: Dockerfile
    build_flag_templates:
      - "--build-arg=VERSION={{ .Version }}"
    extra_files:
      - config.yaml
```

**Anodize:**
```yaml
crates:
  - name: myapp
    # ...
    docker:
      - image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}"
          - "ghcr.io/myorg/myapp:latest"
        dockerfile: Dockerfile
        build_flag_templates:
          - "--build-arg=VERSION={{ Version }}"
        extra_files:
          - config.yaml
        platforms:
          - linux/amd64
          - linux/arm64
        skip_push: false
        push_flags:
          - "--quiet"
```

**Key differences:**
- Docker config is per-crate under `docker[]`.
- `platforms` specifies multi-arch build targets (uses `docker buildx`).
- `skip_push` builds images without pushing to the registry.
- `push_flags` passes additional flags to `docker push`.
- `binaries` specifies which crate binaries to include in the Docker build context.

---

## Pattern 7: Signing

**GoReleaser:**
```yaml
signs:
  - artifacts: checksum
    cmd: gpg
    args:
      - "--batch"
      - "--local-user"
      - "{{ .Env.GPG_FINGERPRINT }}"
      - "--output"
      - "${signature}"
      - "--detach-sig"
      - "${artifact}"
    stdin: "{{ .Env.GPG_PASSWORD }}"

docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - "sign"
      - "${artifact}"
      - "--yes"
```

**Anodize:**
```yaml
signs:
  - id: gpg-sign
    artifacts: checksum
    cmd: gpg
    args:
      - "--batch"
      - "--local-user"
      - "{{ Env.GPG_FINGERPRINT }}"
      - "--output"
      - "{{ Signature }}"
      - "--detach-sig"
      - "{{ Artifact }}"
    stdin: "{{ Env.GPG_PASSWORD }}"
    # stdin_file: /path/to/passphrase  # alternative to stdin

docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - "sign"
      - "{{ Artifact }}"
      - "--yes"
```

**Key differences:**
- `${signature}` and `${artifact}` become `{{ Signature }}` and `{{ Artifact }}` (Tera template syntax).
- `sign:` (singular object) and `signs:` (array) are both accepted.
- `id` field allows referencing specific sign configs.
- `stdin_file` is an alternative to `stdin` for piping file contents.
- `ids` field can limit signing to specific artifact IDs.

---

## Pattern 8: Linux Packages (nFPM)

**GoReleaser:**
```yaml
nfpms:
  - package_name: myapp
    formats:
      - deb
      - rpm
    vendor: "My Company"
    maintainer: "Dev <dev@example.com>"
    description: "My application"
    license: MIT
    dependencies:
      - libc6
    contents:
      - src: config.yaml
        dst: /etc/myapp/config.yaml
        type: config
        file_info:
          mode: 0644
    scripts:
      postinstall: scripts/postinstall.sh
    recommends:
      - curl
    suggests:
      - jq
    conflicts:
      - myapp-legacy
    replaces:
      - myapp-legacy
    provides:
      - myapp
```

**Anodize:**
```yaml
crates:
  - name: myapp
    # ...
    nfpm:
      - package_name: myapp
        formats:
          - deb
          - rpm
        vendor: "My Company"
        maintainer: "Dev <dev@example.com>"
        description: "My application"
        license: MIT
        dependencies:
          deb:
            - libc6
          rpm:
            - glibc
        contents:
          - src: config.yaml
            dst: /etc/myapp/config.yaml
            type: config
            file_info:
              owner: root
              group: root
              mode: "0644"
        scripts:
          postinstall: scripts/postinstall.sh
          preremove: scripts/preremove.sh
        recommends:
          - curl
        suggests:
          - jq
        conflicts:
          - myapp-legacy
        replaces:
          - myapp-legacy
        provides:
          - myapp
```

**Key differences:**
- nFPM config is per-crate under `nfpm[]`.
- `dependencies` is a map keyed by format (e.g., `deb: [libc6]`), not a flat list.
- `file_info` includes `owner` and `group` fields in addition to `mode`.
- `scripts` supports `preinstall`, `postinstall`, `preremove`, `postremove`.
- `overrides` allows per-format config overrides as a map.

---

## Pattern 9: Homebrew and Scoop

**GoReleaser:**
```yaml
brews:
  - tap:
      owner: myorg
      name: homebrew-tap
    folder: Formula
    description: "My application"
    license: MIT
    install: |
      bin.install "myapp"
    test: |
      system "#{bin}/myapp", "--version"

scoops:
  - bucket:
      owner: myorg
      name: scoop-bucket
    description: "My application"
```

**Anodize:**
```yaml
crates:
  - name: myapp
    # ...
    publish:
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
        folder: Formula
        description: "My application"
        license: MIT
        install: |
          bin.install "myapp"
        test: |
          system "#{bin}/myapp", "--version"
      scoop:
        bucket:
          owner: myorg
          name: scoop-bucket
        description: "My application"
```

**Key differences:**
- Homebrew and Scoop are under `crates[].publish`, not top-level.
- Only one Homebrew/Scoop config per crate (GoReleaser allows multiple).

---

## Pattern 10: Hooks and Announcements

**GoReleaser:**
```yaml
before:
  hooks:
    - go mod tidy
    - go generate ./...

after:
  hooks:
    - echo "Release complete"

announce:
  discord:
    enabled: true
    message_template: "{{ .ProjectName }} {{ .Tag }} is out!"
  slack:
    enabled: true
    message_template: "{{ .ProjectName }} {{ .Tag }} released"
```

**Anodize:**
```yaml
before:
  hooks:
    - cargo fmt --check
    - cargo clippy -- -D warnings

after:
  hooks:
    - echo "Release complete"

announce:
  discord:
    enabled: true
    webhook_url: "{{ Env.DISCORD_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} is out!"
  slack:
    enabled: true
    webhook_url: "{{ Env.SLACK_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} released"
  webhook:
    enabled: true
    endpoint_url: "{{ Env.WEBHOOK_URL }}"
    content_type: application/json
    message_template: '{"project":"{{ ProjectName }}","tag":"{{ Tag }}"}'
```

**Key differences:**
- Hooks use Rust toolchain commands instead of Go.
- Discord/Slack require explicit `webhook_url` in config.
- Generic `webhook` provider is available for custom integrations.

---

## Quick Migration Checklist

1. Rename `.goreleaser.yaml` to `.anodize.yaml`.
2. Wrap your build/archive/release/publish config in a `crates[]` entry.
3. Replace `goos`/`goarch` pairs with Rust target triples in `defaults.targets`.
4. Replace `ldflags` with `flags: --release`.
5. Replace `builds[].env` map with per-target env vars in `builds[].env`.
6. Replace `${signature}`/`${artifact}` with `{{ Signature }}`/`{{ Artifact }}` in sign args.
7. Template syntax works as-is (`{{ .Field }}` is auto-converted), but consider migrating to native `{{ Field }}` syntax.
8. Move `brews`/`scoops` to `crates[].publish.homebrew`/`scoop`.
9. Move `nfpms` to `crates[].nfpm`.
10. Move `dockers` to `crates[].docker`.
11. Run `anodize check` to validate your config.
12. Test with `anodize release --snapshot` before doing a real release.

## Workspace Support

The biggest structural difference from GoReleaser is Anodize's native workspace support. If your Go project was a single repo, you'll typically have one entry in `crates[]`. For Rust workspaces with multiple publishable crates, each gets its own entry:

```yaml
crates:
  - name: mylib
    path: crates/mylib
    tag_template: "mylib-v{{ Version }}"
    publish:
      crates: true

  - name: myapp
    path: crates/myapp
    tag_template: "v{{ Version }}"
    depends_on:
      - mylib
    builds:
      - binary: myapp
    release:
      github: {}  # auto-detected
```

Use `depends_on` to ensure crates are published in the correct order. Anodize handles dependency-aware ordering automatically.
