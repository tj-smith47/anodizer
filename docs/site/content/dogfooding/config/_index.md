+++
title = "anodizer.yml config"
description = "Top-level anodizer.yml keys, Tera template helpers, lifecycle hooks, and monorepo configuration."
weight = 40
template = "section.html"
+++

# anodizer.yml config

Top-level configuration keys and the Tera helpers available inside any
template string. Tera syntax is GoReleaser-compatible.

## Live configuration

Top of [`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-24) — every top-level / monorepo / git key in the tables
below is wired here.

```yaml
version: 2
project_name: cfgd
dist: ./dist
report_sizes: true

env:
  - REGISTRY=ghcr.io
  - RELEASE_TYPE=stable

variables:
  repo_url: "https://github.com/tj-smith47/cfgd"
  description: "Declarative, GitOps-style machine configuration management"

git:
  tag_sort: "-version:refname"
  ignore_tags: ["nightly"]
  ignore_tag_prefixes: ["draft-"]
  prerelease_suffix: "-"

tag:
  default_bump: none
  branch_history: full
  tag_prefix: "v"
  release_branches: [master]
  initial_version: "0.3.5"

metadata:
  description: "Declarative, GitOps-style machine configuration management"
  homepage: "https://github.com/tj-smith47/cfgd"
  license: MIT
  maintainers: ["TJ Smith"]
  mod_timestamp: "{{ CommitTimestamp }}"
  full_description: { from_file: README.md }
  commit_author: { name: TJ Smith, email: tj@jarvispro.io }

# 4 workspace entries — independent release cadences, dep-aware ordering.
workspaces:
  - { name: cfgd-core,     crates: [{ name: cfgd-core,     tag_template: "core-v{{ Version }}",     ... }] }
  - { name: cfgd,          crates: [{ name: cfgd,          tag_template: "v{{ Version }}",          depends_on: [cfgd-core], ... }] }
  - { name: cfgd-operator, crates: [{ name: cfgd-operator, tag_template: "operator-v{{ Version }}", depends_on: [cfgd-core], ... }] }
  - { name: cfgd-csi,      crates: [{ name: cfgd-csi,      tag_template: "csi-v{{ Version }}",      depends_on: [cfgd-core], ... }] }

partial:
  by: os
```

## Top-level config

| Key | Status | Notes |
|---|---|---|
| `project_name` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`project_name: anodizer`) |
| `dist` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`dist: ./dist`) |
| `env` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`env: - RELEASE_TYPE=stable`) |
| `env_files` | ✅ Verified | [`crates/core/src/config/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/mod.rs) (`env_files` config field) |
| `variables` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`variables.repo_url` + `.description`) |
| `template_files[]` | ✅ Verified | [`install.sh`](https://github.com/tj-smith47/cfgd/releases/download/v0.3.5/install.sh) (rendered + attached on every cfgd release) |
| `includes[].from_file` | ✅ Verified | [`crates/core/src/config/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/mod.rs) (`IncludeSpec`, parsed from `includes:`) |
| `includes[].from_url` | 🤝 Help wanted | No live config pulls a remote include |
| `before` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`before.hooks` runs `cargo fmt --check`, `clippy`, `test`) |
| `after` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`after.hooks` echo) |
| `build.hooks.pre` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (archive `hooks.before`) |
| `build.hooks.post` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (archive `hooks.after`) |
| `snapshot.name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`snapshot.version_template`) |
| `--auto-snapshot` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (snapshot build on every master push) |
| `nightly.*` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`nightly: {name_template: "cfgd-nightly", tag_name: nightly}`) + [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/nightly.yml) (fired by `cron: '0 4 * * *'`) |
| `metadata.homepage` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.homepage: https://github.com/tj-smith47/cfgd`) |
| `metadata.license` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.license: MIT`) |
| `metadata.description` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.description`) |
| `metadata.maintainers` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.maintainers`) |
| `metadata.mod_timestamp` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`metadata.mod_timestamp: "{{ CommitTimestamp }}"`; applied as mtime of `dist/metadata.json` and `dist/artifacts.json`) |
| `report_sizes` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`report_sizes: true`; prints per-artifact and total sizes in the release summary) |

## Templates

Tera engine, GoReleaser-compatible syntax. Every template string in the
config is rendered.

| Helper | Status | Notes |
|---|---|---|
| `{{ .Field }}` | ✅ Verified | [`crates/core/src/template/vars.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/vars.rs) (every `{{ .Project }}` / `.Version` / `.Tag` / `.Os` / `.Arch` binding) |
| `{{ .Var.* }}` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`{{ Var.repo_url }}` + `{{ Var.description }}`) |
| `{{ .PrefixedTag }}` | ✅ Verified | [`crates/core/src/template/vars.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/vars.rs) (`PrefixedTag` binding) |
| `{{ .Artifacts }}` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`{{ .Artifacts }}` inside `docker_manifests.image_templates`) |
| `{{ .Metadata }}` | ✅ Verified | [`crates/core/src/template/vars.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/vars.rs) (`Metadata` binding) |
| `{{ .IsMerging }}` | ✅ Verified | [`crates/core/src/template/vars.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/vars.rs) (`IsMerging` binding) |
| `{{ .IsRelease }}` | ✅ Verified | [`crates/core/src/template/vars.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/vars.rs) (`IsRelease` binding) |
| String / path / version / env / filter helpers | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`tolower`, `toupper`, `dir`, `base`, `abs`, etc.) |
| `sha*`, `blake2*`, `blake3`, `crc32`, `md5` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`register_hash_fn!` macro) |
| `readFile`, `mustReadFile` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`readFile` / `mustReadFile` registrations) |
| `time`, `.Now.Format` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`time` function + `Now` binding) |
| `mdv2escape` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`mdv2escape` filter) |
| `urlPathEscape` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`urlPathEscape` filter) |
| `in` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`in` filter) |
| `reReplaceAll` | ✅ Verified | [`crates/core/src/template/base_tera.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/template/base_tera.rs) (`reReplaceAll` filter) |

## Monorepo

| Key | Status | Notes |
|---|---|---|
| `monorepo.tag_prefix` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`tag_template: core-v{{ Version }}` / `v{{ Version }}` / `operator-v` / `csi-v`) |
| `monorepo.dir` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`path: crates/cfgd-core`, `crates/cfgd`, `crates/cfgd-operator`, `crates/cfgd-csi`) |
| `cargo_workspace` detection | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (4 workspaces: cfgd-core, cfgd, cfgd-operator, cfgd-csi) |
| `depends_on` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`depends_on: [cfgd-core]` on the three downstream crates) |

## Publisher resilience

| Key | Status | Notes |
|---|---|---|
| `publish.on_error` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`defaults.publish.on_error` runs a `cmd` per failed publisher before rollback; template vars `.Publisher`/`.Error`/`.Version`/`.Tag`/`.Group`/`.Required`/`.RolledBack`). Workspace-wide; per-crate entries append before defaults. Awaits a real failure to prove live |
| `defaults.publish.cargo.retain_on_rollback` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retain_on_rollback: true` under `defaults.publish.cargo` — crates.io publishes are permanent; retain even if a downstream publisher rolls back) |
| `schemastore.retain_on_rollback` / `mcp.retain_on_rollback` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retain_on_rollback: true` on the top-level `schemastore` and `mcp` keys — external catalogs; retain even if downstream publishers roll back) |

## Tag configuration

| Key | Status | Notes |
|---|---|---|
| `tag.default_bump` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`default_bump: none` — chore/docs/ci-only ranges produce no release) |
| `tag.bump_minor_pre_major` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`bump_minor_pre_major: true` — breaking changes stay in 0.x until 1.0 is deliberate) |
| `tag.tag_prefix` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`tag_prefix: "v"`) |
| `tag.release_branches` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`release_branches: [main, master]`) |
| `tag.initial_version` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`initial_version: "0.3.5"`) |
| `git.tag_sort` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tag_sort: smartsemver`) |
| `git.ignore_tag_prefixes` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`ignore_tag_prefixes: ["draft-"]`) |
| `git.prerelease_suffix` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) + [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`prerelease_suffix: "-"` — strips trailing pre-release suffixes from version strings) |
| `git.ignore_tags` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) + [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`ignore_tags: ["nightly"]` — excludes transient tags from version resolution) |
| `version_files` | ⏳ Pending | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`version_files: [docs/installation.md, chart/cfgd/Chart.yaml]`; version string rewritten in-place at tag time). Wired in config; awaits next cfgd release for live proof |

## Defaults

| Key | Status | Notes |
|---|---|---|
| `defaults.targets` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (6 targets: linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64/aarch64) |
| `defaults.cross` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cross: auto`) |
| `defaults.builds.flags` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`flags: [--release]`) |
| `defaults.archives.formats` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`formats: [tar.gz]`, `format_overrides: windows→zip`) |
| `defaults.archives.hooks` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`hooks.before` + `hooks.after` with Tera vars) |
| `defaults.checksum.algorithm` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`algorithm: sha256`) |
| `defaults.publish.cargo` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cargo: {}` — presence opts every crate into crates.io) |
| `defaults.publish.on_error` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`on_error: [{cmd: "echo ..."}]`) |
| `partial.by` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) + [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`partial.by: os` — shards the CI matrix by OS; enables the determinism fan-out build strategy) |

## Changelog

| Key | Status | Notes |
|---|---|---|
| `changelog.use` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`use: git`) |
| `changelog.title` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`title: "Changelog"`) |
| `changelog.header` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`header: "# Changelog for {{ ProjectName }}"`) |
| `changelog.footer` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`footer: "_Generated by anodizer._"`) |
| `changelog.sort` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`sort: asc`) |
| `changelog.abbrev` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`abbrev: 12`) |
| `changelog.format` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`format: "* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})"`) |
| `changelog.divider` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`divider: "---"`) |
| `changelog.filters.exclude` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (excludes `^docs:`, `^ci:`, `^chore:`, `^style:`) |
| `changelog.groups` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (Features/Bug Fixes/Performance/Others groups with `regexp` + `order`) |
| `changelog.files.per_crate` | ⏳ Pending | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`changelog.files.per_crate: true`) |

## Build artifacts

| Key | Status | Notes |
|---|---|---|
| `source.enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `source.format` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`format: tar.gz`) |
| `source.name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}-{{ Version }}-source"`) |
| `source.prefix_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}-{{ Version }}/"`) |
| `source.files` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (globs: `crates/**/*.rs`, `Cargo.toml`, `Cargo.lock`, `LICENSE`, `README.md`) |
| `sboms[].id` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`id: default`) |
| `sboms[].documents` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ .ArtifactName }}.cdx.json"`) |
| `sboms[].artifacts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`artifacts: archive`) |
| `upx[].enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `upx[].binary` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`binary: upx`) |
| `upx[].args` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`["--best", "--lzma"]`) |
| `upx[].compress` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`compress: "9"`) |
| `upx[].lzma` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`lzma: true`) |
| `upx[].targets` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (4 targets; excludes macOS ARM + Windows ARM — UPX unsupported there) |
| `binstall.enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true` — per-target `pkg_url` overrides auto-derived from archive `name_template`) |
| `checksum.name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ArtifactName }}.sha256"`) |
| `checksum.split` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`split: true` — one `.sha256` sidecar per artifact instead of a combined file) |

## Signing

| Key | Status | Notes |
|---|---|---|
| `signs[].id` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`id: default`) |
| `signs[].artifacts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`artifacts: checksum` — GPG signs each `.sha256` sidecar) |
| `signs[].cmd` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cmd: gpg`) |
| `signs[].args` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`--batch --local-user {{ Env.GPG_FINGERPRINT }} --output {{ Signature }} --detach-sig {{ Artifact }}`) |
| `signs[].if` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (skips in snapshot mode; runs in harness mode for determinism proof) |
| `binary_signs[].artifacts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`artifacts: binary` — cosign signs each binary blob) |
| `binary_signs[].cmd` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cmd: cosign sign-blob --key=env://COSIGN_KEY --bundle={{ Signature }} --yes {{ Artifact }}`) |
| `binary_signs[].if` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (same snapshot/harness guard as `signs`) |
| `docker_signs[].artifacts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`artifacts: manifests` — cosign signs OCI manifests) |
| `docker_signs[].cmd` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cmd: cosign sign --key=env://COSIGN_KEY --yes {{ Artifact }}`) |
| `docker_signs[].if` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (same snapshot/harness guard) |

## Packaging

| Key | Status | Notes |
|---|---|---|
| `nfpm[].id` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`id: default`) |
| `nfpm[].formats` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`formats: [deb, rpm, apk]`) |
| `nfpm[].vendor/maintainer/homepage` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (metadata fields propagated to all three formats) |
| `nfpm[].bindir` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`bindir: /usr/bin`) |
| `nfpm[].section/priority/epoch/release/umask` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (Debian/RPM packaging metadata) |
| `nfpm[].mtime` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`mtime: "{{ CommitDate }}"` — reproducible package mtime) |
| `nfpm[].recommends/suggests/provides` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (recommends git, suggests upx, provides anodizer) |
| `nfpm[].file_name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}_{{ RawVersion }}_{{ Os }}_{{ Arch }}"`) |
| `nfpm[].contents` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (LICENSE + README.md installed to `/usr/share/doc/anodizer/`) |
| `nfpm[].deb.signature.key_file` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ .Env.GPG_KEY_PATH }}"`, type: origin) |
| `nfpm[].rpm.signature.key_file` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (same GPG key; also sets `group` + `packager`) |
| `nfpm[].apk.signature.key_file` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ .Env.APK_PRIVATE_KEY_PATH }}"` — RSA-PSS, not OpenPGP) |
| `snapcrafts[].name/title/summary/description` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (live in v0.7.0 — 2026-06-09; amd64 + arm64 revisions published to Snap Store) |
| `snapcrafts[].base` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`base: core24`; live in v0.7.0) |
| `snapcrafts[].grade/confinement` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`grade: stable`, `confinement: strict`; live in v0.7.0) |
| `snapcrafts[].publish` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`publish: true`; live in v0.7.0) |
| `snapcrafts[].channel_templates` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`["latest/stable"]`; live in v0.7.0) |
| `snapcrafts[].apps` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (app with `home`, `network`, `network-bind` plugs; live in v0.7.0) |
| `appimages[].desktop` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`assets/anodizer.desktop`) |
| `appimages[].icon` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`assets/logo.png`) |
| `appimages[].filename` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}-{{ Version }}-{{ Arch }}.AppImage"`) |
| `appimages[].update_information` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`gh-releases-zsync|...|*.AppImage.zsync` for delta updates) |
| `makeselfs[].id` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`id: default`) |
| `makeselfs[].name` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"anodizer Installer"`) |
| `makeselfs[].script` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`scripts/makeself-install.sh`) |
| `makeselfs[].filename` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}-installer.run"`) |
| `srpm.enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `srpm.package_name` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`package_name: anodizer`) |
| `srpm.spec_file` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`spec_file: anodizer.spec`) |
| `srpm.file_name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }}-{{ RawVersion }}-1.src.rpm"`) |

## Release

| Key | Status | Notes |
|---|---|---|
| `release.github.owner/name` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`owner: tj-smith47`, `name: anodizer`) |
| `release.draft` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`draft: false`) |
| `release.prerelease` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`prerelease: auto`) |
| `release.make_latest` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`make_latest: auto`) |
| `release.mode` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`mode: keep-existing`) |
| `release.target_commitish` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ .Commit }}"`) |
| `release.discussion_category_name` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"Announcements"`) |
| `release.replace_existing_draft` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`replace_existing_draft: true`) |
| `release.replace_existing_artifacts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`replace_existing_artifacts: true`) |
| `release.name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ ProjectName }} {{ Tag }}"`) |
| `release.header` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"## What's new in {{ .Tag }}"`) |
| `release.footer` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (credits anodizer with 🦀) |
| `release.include_meta` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`include_meta: true`) |
| `release.extra_files` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (APK signing public key + man page, both with `allow_empty: true`) |

## Package managers

| Key | Status | Notes |
|---|---|---|
| `publish.cargo` (via defaults) | ✅ Verified | [anodizer crates.io](https://crates.io/crates/anodizer) (all workspace crates published in dependency order) |
| `publish.cargo.wait_for_workspace_deps` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (inherited from `defaults.publish.cargo: {}` — waits for sparse index propagation) |
| `publish.cargo.retain_on_rollback` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retain_on_rollback: true` in defaults.publish.cargo) |
| `publish.aur.git_url` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`ssh://aur@aur.archlinux.org/anodizer-bin.git`) |
| `publish.aur.name/description/license/depends` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `publish.aur.private_key` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`AUR_SSH_KEY` secret) |
| `publish.nix.repository` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tj-smith47/nix-pkgs`) |
| `publish.nix.formatter` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`formatter: alejandra`) |
| `publish.nix.extra_install/post_install` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (installs man page; echoes setup hint) |
| `publish.scoop.repository` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tj-smith47/scoop-bucket`) |
| `publish.scoop.depends/shortcuts` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (depends: git; shortcuts: anodizer.exe) |
| `publish.winget.package_identifier` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`TJSmith.Anodizer`) |
| `publish.winget.dependencies` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`Microsoft.VCRedist.2015+.x64` — required for MSVC binaries) |
| `publish.winget.update_existing_pr` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`update_existing_pr: true`) |
| `publish.chocolatey.title/summary/authors/owners` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `publish.chocolatey.republish_in_moderation` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`republish_in_moderation: true`) |
| `homebrew_casks[].repository/directory` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tj-smith47/homebrew-tap`, `Casks/`) |
| `homebrew_casks[].binaries` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`binaries: [anodizer]`) |
| `homebrew_casks[].generate_completions_from_executable` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (calls `anodizer completion <shell>` at cask install for bash/zsh/fish) |
| `homebrew_casks[].manpages` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`anodizer.1` downloaded from release extra_files) |
| `homebrew_casks[].caveats` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) ("Run `anodizer init`...") |
| `homebrew_casks[].dependencies` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`formula: git`) |

## Distributions

| Key | Status | Notes |
|---|---|---|
| `blobs[].provider` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`provider: s3`; arc-anodizer runner has cluster-internal access to MinIO) |
| `blobs[].bucket` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`anodizer-releases`) |
| `blobs[].endpoint` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`"{{ Env.MINIO_ENDPOINT }}"` → `http://minio.jarvispro.svc.cluster.local:9003`) |
| `blobs[].region` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`region: us-east-1` — compatibility placeholder; MinIO ignores region) |
| `blobs[].s3_force_path_style` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`s3_force_path_style: true` — required for MinIO path-style addressing) |
| `blobs[].disable_ssl` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`disable_ssl: true` — the in-cluster MinIO endpoint is plain http; without it the S3 client rejects the non-https endpoint) |
| `blobs[].directory` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`directory: "{{ Tag }}"`) |
| `cloudsmiths[].organization` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`organization: jarvispro`) |
| `cloudsmiths[].repository` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`repository: anodizer`) |
| `cloudsmiths[].formats` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`[deb, rpm, alpine]`) |
| `cloudsmiths[].distributions` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (deb/rpm: any-distro/any-version; alpine: alpine/any-version) |
| `cloudsmiths[].republish` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`republish: true` — prevents MD5 conflict on re-cut) |
| `dockerhub[].username` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — DockerHub repo not yet created; description-sync only, not image publishing) |
| `dockerhub[].description` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (configured but disabled via `skip: true`) |
| `artifactories[].target` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no Artifactory instance) |
| `artifactories[].method` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`method: PUT`, disabled) |
| `npms[].scope` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no NPM_TOKEN) |
| `npms[].metapackage/bin/mode` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`mode: optional-deps` — biome/git-cliff pattern; wired, awaits token) |
| `mcp.name/title/description/homepage` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `mcp.repository` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`source: github`) |
| `mcp.packages` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`registry_type: oci`, `identifier: ghcr.io/tj-smith47/anodizer`, `transport: stdio`) |
| `mcp.auth` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`auth.type: github-oidc`; requires `id-token: write` on release job) |
| `mcp.retain_on_rollback` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retain_on_rollback: true`) |
| `schemastore.repository` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tj-smith47/schemastore`) |
| `schemastore.schemas` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (matches `.anodizer.yaml` + `.anodizer.yml`; URL: `tj-smith47.github.io/anodizer/schema.json`) |
| `schemastore.retain_on_rollback` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retain_on_rollback: true`) |

## Post-release

| Key | Status | Notes |
|---|---|---|
| `verify_release.enabled` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `verify_release.assert_assets` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`assert_assets: true` — every produced artifact must appear as an uploaded release asset) |
| `verify_release.install_smoke` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`install_smoke: {}` — installs each nfpm package in a container and runs `<bin> --version`). Auto-detects the Docker topology (bind-mount, or `docker cp` under a separate-filesystem dind). Also configured in [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) |
| `verify_release.glibc_ceiling` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`glibc_ceiling: "2.36"` — fails if any `.deb` requires glibc > 2.36) |
| `attestations.enabled` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `attestations.mode` | ⏳ Pending | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`mode: subjects` — writes `dist/attestation-subjects.json`; anodizer-action feeds it to `actions/attest-build-provenance`) |
| `attestations.artifacts` | 🤝 Help wanted | Not configured — defaults to all artifact kinds when `mode: subjects` |
| `milestones[].repo` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`tj-smith47/anodizer`) |
| `milestones[].close` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`close: false` — wired but disabled; no milestones configured) |
| `milestones[].fail_on_error` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`fail_on_error: true` — milestone errors fail the release instead of vanishing) |
| `milestones[].name_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`name_template: "{{ Tag }}"`) |

## Announce

| Key | Status | Notes |
|---|---|---|
| `announce.gate_on` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`gate_on: required_publishers` — announce only after all required publishers succeed) |
| `announce.webhook.enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true` — posts JSON to `tj.jarvispro.io/webhooks/anodizer`) |
| `announce.webhook.endpoint_url` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `announce.webhook.content_type` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`application/json`) |
| `announce.webhook.message_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (JSON payload with `project`, `tag`, `url`) |
| `announce.webhook.headers` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`X-Anodizer-Source: release`) |
| `announce.webhook.skip_tls_verify` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip_tls_verify: false`) |
| `announce.webhook.expected_status_codes` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`[200, 202]`) |
| `announce.email.enabled` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: true`) |
| `announce.email.host/port/username/from/to` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (Gmail SMTP, port 587, STARTTLS) |
| `announce.email.subject_template` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `announce.email.encryption` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`encryption: starttls`) |
| `announce.discord` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.slack` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.telegram` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.teams` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.mattermost` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.reddit` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.twitter` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.mastodon` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.bluesky` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.linkedin` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.discourse` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |
| `announce.opencollective` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`enabled: false`) |

## Platform-specific (disabled)

| Key | Status | Notes |
|---|---|---|
| `flatpaks[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no Flatpak runtime configured; `app_id: io.github.tj_smith47.Anodizer`, runtime: `org.freedesktop.Platform/24.08`) |
| `app_bundles[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no macOS app bundle signing identity) |
| `dmgs[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — needs app_bundle first) |
| `pkgs[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no macOS PKG signing identity) |
| `msis[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no WiX source `.wxs`) |
| `nsis[].skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no NSIS script) |
| `notarize.skip` | 🤝 Help wanted | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`skip: true` — no Apple Developer credentials) |
