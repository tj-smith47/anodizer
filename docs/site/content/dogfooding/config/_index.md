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
| `nightly.*` | 🤝 Help wanted | Wired; no scheduled nightly workflow yet |
| `metadata.homepage` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.homepage: https://github.com/tj-smith47/cfgd`) |
| `metadata.license` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.license: MIT`) |
| `metadata.description` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.description`) |
| `metadata.maintainers` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`metadata.maintainers`) |
| `metadata.mod_timestamp` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`metadata.mod_timestamp: "{{ CommitTimestamp }}"`; applied as mtime of `dist/metadata.json` and `dist/artifacts.json`) |

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
| `git.tag_sort` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`git.tag_sort: "-version:refname"`) |
| `git.prerelease_suffix` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`git.prerelease_suffix: "-"`) |
| `git.ignore_tags` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`git.ignore_tags: ["nightly"]`) |
| `partial.by` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`partial.by: os` at file end) |
