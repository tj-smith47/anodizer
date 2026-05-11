+++
title = "anodizer.yml config"
description = "Top-level anodizer.yml keys, Tera template helpers, lifecycle hooks, and monorepo configuration."
weight = 40
template = "section.html"
+++

# anodizer.yml config

Top-level configuration keys and the Tera helpers available inside any
template string. Tera syntax is GoReleaser-compatible.

## Top-level config

| Key | Status | Notes |
|---|---|---|
| `project_name` | ✅ Verified | Used in every config |
| `dist` | ✅ Verified | Used in every config |
| `env` | ✅ Verified | Used in every config |
| `env_files` | ✅ Verified | Used in every config |
| `variables` | ✅ Verified | cfgd uses `.Var.*` heavily |
| `template_files[]` | ✅ Verified | cfgd renders an `install.sh` and ships it |
| `includes[].from_file` | ✅ Verified | Wired |
| `includes[].from_url` | 🤝 Help wanted | No live config pulls a remote include |
| `before` | ✅ Verified | cfgd uses global before-hooks |
| `after` | ✅ Verified | cfgd uses global after-hooks |
| `build.hooks.pre` | ✅ Verified | Wired |
| `build.hooks.post` | ✅ Verified | Wired |
| `snapshot.name_template` | ✅ Verified | Snapshot job on every master push |
| `--auto-snapshot` | ✅ Verified | Snapshot job on every master push |
| `nightly.*` | 🤝 Help wanted | Wired; no scheduled nightly workflow yet |
| `metadata.homepage` | ✅ Verified | Collected and emitted |
| `metadata.license` | ✅ Verified | Collected and emitted |
| `metadata.description` | ✅ Verified | Collected and emitted |
| `metadata.maintainers` | ✅ Verified | Collected and emitted |
| `metadata.mod_timestamp` | 🤝 Help wanted | Collected; minor field-passthrough gaps |

## Templates

Tera engine, GoReleaser-compatible syntax. Every template string in the
config is rendered.

| Helper | Status | Notes |
|---|---|---|
| `{{ .Field }}` | ✅ Verified | project, version, tag, os, arch, etc. Every shipped asset filename is template-rendered |
| `{{ .Var.* }}` | ✅ Verified | cfgd uses `.Var.repo_url` and `.Var.description` across its config |
| `{{ .PrefixedTag }}` | ✅ Verified | Pro template variable, wired |
| `{{ .Artifacts }}` | ✅ Verified | cfgd uses `.Artifacts` in `docker_manifests` |
| `{{ .Metadata }}` | ✅ Verified | Pro template variable, wired |
| `{{ .IsMerging }}` | ✅ Verified | Pro template variable, wired |
| `{{ .IsRelease }}` | ✅ Verified | Pro template variable, wired |
| String / path / version / env / filter helpers | ✅ Verified | Wired |
| `sha*`, `blake2*`, `blake3`, `crc32`, `md5` | ✅ Verified | Hash helpers wired |
| `readFile`, `mustReadFile` | ✅ Verified | File I/O wired |
| `time`, `.Now.Format` | ✅ Verified | Date helpers wired |
| `mdv2escape` | ✅ Verified | Telegram MarkdownV2 escape, wired |
| `urlPathEscape` | ✅ Verified | Wired |
| `in` | ✅ Verified | Pro helper, wired |
| `reReplaceAll` | ✅ Verified | Pro helper, wired |

## Monorepo

| Key | Status | Notes |
|---|---|---|
| `monorepo.tag_prefix` | ✅ Verified | cfgd uses `core-v*`, `v*`, `operator-v*`, `csi-v*` |
| `monorepo.dir` | ✅ Verified | Per-crate dir mapping in cfgd's 4-crate workspace |
| `cargo_workspace` detection | ✅ Verified | cfgd is a 4-crate workspace (CLI + lib + operator + CSI). All four release in parallel |
| `depends_on` | ✅ Verified | cfgd's `core` releases first, others after |
| `git.tag_sort` | ✅ Verified | Wired |
| `git.prerelease_suffix` | ✅ Verified | Wired |
| `git.ignore_tags` | ✅ Verified | Wired |
| `partial.by` | ✅ Verified | cfgd uses `partial.by: goos`. Axis: goos / goarch / target |
