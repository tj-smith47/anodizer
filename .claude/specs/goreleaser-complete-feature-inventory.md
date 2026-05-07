# GoReleaser Complete Feature Inventory

> **Authoritative parity reference** for anodizer v0.x ↔ GoReleaser.
> Source: `/opt/repos/goreleaser/` (OSS, at HEAD — last sync commit `8976559`, fetched 2026-05-07; tag `v2.16.0-17315a55-nightly-1-g8976559`; prior sync was `f7e73e3` 2026-04-16).
> Pro: `https://goreleaser.com/pro/` + `https://goreleaser.com/customization/*` — refreshed 2026-05-07.
> Anodizer ground truth: `/opt/repos/anodizer/crates/` (grepped for `implemented` status).
>
> **2026-05-07 refresh.** Walked 50 commits in v2.15.0..HEAD. New rows + downgrades captured in §2.18 Refresh delta and inline with `2026-05-07` evidence dates. Net: 1 new pipe (`mcp` reclassified), 1 new builder (`node` SEA, n-a), 1 new archive format (`xz` single-file), 1 new global config (`retry:`), and ~10 behavioral fixes — see §5.delta.
>
> **How to read this file.** The Parity Row Matrix (Section 2) is the audit-driving surface. One row per feature/feature-group. Columns:
> - `name` — feature identifier (config key or conceptual name)
> - `category` — area bucket (build, archive, sign, publish-<channel>, announce-<provider>, release, changelog, docker, sbom, blob, source, metadata, hooks, cli, partial, template-helpers, misc)
> - `tier` — OSS | Pro
> - `scope` — portable | go-specific | rust-additive | rust-native-replacement
> - `ecosystem_relevance` — required | strongly-suggested | niche | not-applicable (see decision rule at bottom)
> - `parity_status` — implemented | partial | missing | n-a
> - `disposition` — `—` default; set to `remove` | `repurpose` | `hide` | `keep` only when `parity_status=implemented AND ecosystem_relevance=not-applicable` AND the row in `audits/2026-04-v0.x/bloat.md` has a second-reviewer countersign
> - `source_ref` — file:line in `/opt/repos/goreleaser/` for OSS, or docs URL for Pro
> - `notes` — ≤30 word durable justification
>
> Reference tables (fields, defaults, environment variables, CLI flags) preserved in Section 6.

---

## 1. Parity Definition

Parity = equal or superior implementation per GoReleaser feature: config field, behavior, wiring, error, auth, default. Parsed-but-ignored fields are `partial`. Fields with different semantics are `partial` unless anodizer's divergence is an intentional, documented superiority.

---

## 2. Parity Row Matrix — CLI

### 2.1 Builds

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| builder: go | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/ | Go toolchain builder; Rust uses cargo, not portable. |
| builder: rust | build | OSS | portable | required | implemented | — | internal/builders/rust/build.go | anodizer is the native Rust releaser — cargo/cross/zigbuild via `stage-build`. |
| builder: zig | build | OSS | rust-additive | niche | n-a | — | internal/builders/zig/ | Out of scope for Rust; cargo-zigbuild covers zig-as-linker for Rust targets. |
| builder: bun | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/bun/ | JS/TS runtime builder; no Rust analogue. |
| builder: deno | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/deno/ | JS/TS runtime builder; no Rust analogue. |
| builder: python-uv | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/uv/ | Python packaging builder; no Rust analogue. |
| builder: python-poetry | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/poetry/ | Python packaging builder; no Rust analogue. |
| builder: prebuilt | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/base/ | anodizer `copy_from` + `import` equivalents in `crates/stage-build/src/lib.rs`. |
| build.id | build | OSS | portable | required | implemented | — | internal/config/config.go | `CrateConfig.id` (core/config.rs:738). |
| build.binary | build | OSS | portable | required | implemented | — | internal/config/config.go | `BuildConfig.binary`. |
| build.main | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go entrypoint path; Rust uses `--bin`/`Cargo.toml`. |
| build.dir | build | OSS | portable | required | implemented | — | internal/builders/base/ | `BuildConfig.dir`. |
| build.command | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/base/ | anodizer uses `cargo <command>` — defaults `build`/`zigbuild`. |
| build.flags | build | OSS | portable | required | implemented | — | internal/builders/base/ | `BuildConfig.flags` (default `--release`). |
| build.ldflags | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go linker flags; Rust uses `RUSTFLAGS`+`build.rs`. |
| build.asmflags | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go asm flags; no Rust analogue. |
| build.gcflags | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go compiler flags; Rust uses `[profile.*]` in Cargo.toml. |
| build.tags | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go build tags; Rust uses `--features`. |
| build.env | build | OSS | portable | required | implemented | — | internal/builders/base/ | `BuildConfig.env` templated. |
| build.tool | build | OSS | portable | required | implemented | — | internal/builders/rust/build.go | anodizer resolves `cargo`/`cross` via `CrossStrategy`. |
| build.goos / goarch / goarm / goamd64 / goarm64 / gomips / go386 / goppc64 / goriscv64 | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go matrix metadata; Rust uses target triples via `targets`. |
| build.targets | build | OSS | portable | required | implemented | — | internal/builders/rust/build.go:20 | Rust target triples in `CrateConfig.targets` + `Defaults.targets`. |
| build.ignore | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/base/ | Go goos/goarch exclusions; Rust uses explicit target list. |
| build.buildmode | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go buildmode (c-shared/c-archive); not in Rust scope. |
| build.mod_timestamp | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/base/ | `CrateConfig.mod_timestamp` wired in `stage-build/src/lib.rs`. |
| build.overrides | build | OSS | portable | required | implemented | — | internal/builders/base/ | `BuildOverride` array wired per-target. |
| build.hooks.pre/post | build | OSS | portable | required | implemented | — | internal/builders/base/ | `BuildHooksConfig` + `run_hooks` in core. |
| build.skip | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/base/ | `BuildConfig.skip` templated bool. |
| build.no_unique_dist_dir | build | OSS | portable | niche | implemented | — | internal/builders/base/ | Wired in stage-build. |
| build.no_main_check | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go | Go-specific `main` package check. |
| gomod.proxy | build | OSS | go-specific | not-applicable | n-a | — | internal/pipe/gomod/ | Go proxy integration; Rust-native replacement is `Cargo.lock` fidelity. |
| gomod.env / mod / gobinary / dir | build | OSS | go-specific | not-applicable | n-a | — | internal/pipe/gomod/ | Go module proxy env; see Rust-additive §3 `Cargo.lock` / `cargo metadata`. |
| universal_binaries (macOS) | build | OSS | portable | strongly-suggested | implemented | — | internal/pipe/universalbinary/ | `UniversalBinaryConfig` wired via `lipo` subprocess in stage-build. |
| upx | build | OSS | portable | niche | implemented | — | internal/pipe/upx/upx.go | `stage-upx/src/lib.rs`; uses Rust target-triple globs (not goos/goarch). |
| partial builds (`--single-target`) | build | OSS | portable | strongly-suggested | implemented | — | internal/pipe/partial/partial.go | `cli --single-target` flag in `commands/build.rs`. |
| prebuild pipe | build | OSS | portable | niche | implemented | — | internal/pipe/prebuild/prebuild.go | Pre-build validation + prepare hooks; folded into anodizer build stage. |
| reportsizes | build | OSS | portable | strongly-suggested | implemented | — | internal/pipe/reportsizes/reportsizes.go | Binary-size reporter after build. |

### 2.2 Archives

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| archives[].id / ids | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | `ArchiveConfig.id`/`ids`. |
| archives[].format (singular, deprecated) | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | Legacy field; anodizer accepts both. |
| archives[].formats (plural) | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | v2.6+ plural forms list. |
| archives[].meta (manifest-only) | archive | OSS | portable | niche | implemented | — | internal/pipe/archive/archive.go | `ArchiveConfig.meta`. |
| archives[].name_template | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | Full Tera-backed template. |
| archives[].wrap_in_directory | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | bool/string + template support. |
| archives[].strip_binary_directory | archive | OSS | portable | niche | implemented | — | internal/pipe/archive/archive.go | Wired in stage-archive. |
| archives[].allow_different_binary_count | archive | OSS | portable | niche | implemented | — | internal/pipe/archive/archive.go | Wired in stage-archive. |
| archives[].files (string/object) | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | `ArchiveFileSpec` enum parses both shapes. |
| archives[].builds_info | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | File mode/owner/group/mtime on built binaries. |
| archives[].format_overrides | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | Plural `formats` + `goos` override key. |
| archives[].binaries (anodizer-only) | archive | — | portable | niche | implemented | — | — | Anodizer extension, no GoReleaser equivalent. Filters which build binaries enter this archive by file-name match. Silently **intersects** with `ids:` when both are set — configure only one. Prefer `ids:` for parity with GoReleaser. |
| archives[].hooks.before/after | archive | Pro | portable | strongly-suggested | implemented | — | docs: /customization/archive/ (fetched 2026-04-16) | `BuildHooksConfig.pre`/`post` with `#[serde(alias="before")]`/`alias="after"` (config.rs:979,982). Verified 2026-04-18. |
| archives[].templated_files | archive | Pro | portable | niche | implemented | — | docs: /customization/archive/ (fetched 2026-04-16) | `templated_files` via `TemplatedExtraFile`. |
| formats: tar.gz / tgz | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | `stage-archive`. |
| formats: tar.xz / txz | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | `stage-archive`. |
| formats: tar.zst / tzst | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | `stage-archive` (v2.1+). |
| formats: tar | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | `stage-archive`. |
| formats: gz | archive | OSS | portable | niche | implemented | — | internal/pipe/archive/archive.go | Single-file gzip. |
| formats: zip | archive | OSS | portable | required | implemented | — | internal/pipe/archive/archive.go | `stage-archive` — Windows default. |
| formats: binary | archive | OSS | portable | strongly-suggested | implemented | — | internal/pipe/archive/archive.go | Passthrough of raw binary. |
| formats: none | archive | OSS | portable | niche | implemented | — | internal/pipe/archive/archive.go | Skip archive creation. |
| source archive | source | OSS | portable | required | implemented | — | internal/pipe/sourcearchive/ | `stage-source/src/lib.rs`. |
| source.templated_files | source | Pro | portable | niche | implemented | — | docs: /customization/source/ (fetched 2026-04-16) | Wired. |

### 2.3 Checksums

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| checksum.name_template | checksum | OSS | portable | required | implemented | — | internal/pipe/checksums/checksums.go | `ChecksumConfig.name_template`. |
| checksum.algorithm | checksum | OSS | portable | required | implemented | — | internal/pipe/checksums/checksums.go | sha256 default; supports all listed algos. |
| checksum.split (per-artifact sidecar) | checksum | OSS | portable | strongly-suggested | implemented | — | internal/pipe/checksums/checksums.go | `ChecksumConfig.split`. |
| checksum.disable | checksum | OSS | portable | strongly-suggested | implemented | — | internal/pipe/checksums/checksums.go | StringOrBool. |
| checksum.ids | checksum | OSS | portable | strongly-suggested | implemented | — | internal/pipe/checksums/checksums.go | Filter. |
| checksum.extra_files | checksum | OSS | portable | strongly-suggested | implemented | — | internal/pipe/checksums/checksums.go | Includes external glob files. |
| checksum.templated_extra_files | checksum | Pro | portable | niche | implemented | — | docs: /customization/checksum/ (fetched 2026-04-16) | Pro feature, wired. |
| algorithms: sha256/512/1/224/384 | checksum | OSS | portable | required | implemented | — | internal/pipe/checksums/ | stage-checksum. |
| algorithms: sha3-256/512/224/384 | checksum | OSS | portable | niche | implemented | — | internal/pipe/checksums/ | stage-checksum. |
| algorithms: blake2b/2s/3 | checksum | OSS | portable | niche | implemented | — | internal/pipe/checksums/ | stage-checksum. |
| algorithms: crc32 / md5 | checksum | OSS | portable | niche | implemented | — | internal/pipe/checksums/ | stage-checksum. |

### 2.4 Release (SCM)

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| release.github | release | OSS | portable | required | implemented | — | internal/pipe/release/ | GitHub API in `github_client.rs`. |
| release.gitlab | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/ | GitLab client wired in core. |
| release.gitea | release | OSS | portable | niche | implemented | — | internal/pipe/release/ | Gitea client wired. |
| release.draft | release | OSS | portable | required | implemented | — | internal/pipe/release/release.go | `ReleaseConfig.draft`. |
| release.replace_existing_draft | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired in stage-release. |
| release.use_existing_draft | release | OSS | portable | niche | implemented | — | internal/pipe/release/release.go | v2.5+ — wired. |
| release.replace_existing_artifacts | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.target_commitish | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.tag (template) | release | Pro | portable | strongly-suggested | implemented | — | docs: /customization/release/ (fetched 2026-04-16) | `ReleaseConfig.tag` templated. |
| release.discussion_category_name | release | OSS | portable | niche | implemented | — | internal/pipe/release/release.go | Wired. |
| release.prerelease (auto/bool) | release | OSS | portable | required | implemented | — | internal/pipe/release/release.go | `PrereleaseConfig` enum. |
| release.make_latest | release | OSS | portable | required | implemented | — | internal/pipe/release/release.go | v2.6+, `MakeLatestConfig`. |
| release.mode (keep-existing/append/prepend/replace) | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.header / footer (string) | release | OSS | portable | required | implemented | — | internal/pipe/release/release.go | Wired. |
| release.header.from_url / from_file | release | Pro | portable | strongly-suggested | implemented | — | docs: /customization/release/ | `ContentSource::FromUrl { from_url, headers }` (config.rs:1376); body template-rendered in stage-release/src/lib.rs:631. Verified 2026-04-18. |
| release.footer.from_url / from_file | release | Pro | portable | strongly-suggested | implemented | — | docs: /customization/release/ | Same wiring as header via shared `ContentSource::FromUrl`. Verified 2026-04-18. |
| release.name_template | release | OSS | portable | required | implemented | — | internal/pipe/release/release.go | Wired. |
| release.disable | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.skip_upload | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.extra_files | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Wired. |
| release.templated_extra_files | release | Pro | portable | niche | implemented | — | docs: /customization/release/ | Wired. |
| release.include_meta | release | OSS | portable | niche | implemented | — | internal/pipe/release/release.go | Wired (Session G verified). |
| github_urls.api / upload / download / skip_tls_verify | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/release/release.go | Enterprise URL overrides wired; `skip_tls_verify` in client. |
| gitlab_urls.* | release | OSS | portable | niche | implemented | — | internal/pipe/release/release.go | GitLab URL overrides wired. |
| gitea_urls.* | release | OSS | portable | niche | implemented | — | internal/pipe/release/release.go | Gitea URL overrides wired. |
| milestone pipe | release | OSS | portable | strongly-suggested | implemented | — | internal/pipe/milestone/ | `commands/release/milestones.rs`. |

### 2.5 Changelog

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| changelog.disable | changelog | OSS | portable | required | implemented | — | internal/pipe/changelog/changelog.go | StringOrBool. |
| changelog.use (git/github/gitlab/gitea/github-native) | changelog | OSS | portable | required | implemented | — | internal/pipe/changelog/changelog.go | Provider-switching wired. |
| changelog.format (template) | changelog | OSS | portable | strongly-suggested | implemented | — | internal/pipe/changelog/changelog.go | Per-entry template. |
| changelog.sort (asc/desc) | changelog | OSS | portable | required | implemented | — | internal/pipe/changelog/changelog.go | Wired. |
| changelog.abbrev | changelog | OSS | portable | strongly-suggested | implemented | — | internal/pipe/changelog/changelog.go | 0 / -1 / N. |
| changelog.paths (monorepo filter) | changelog | Pro | portable | niche | implemented | — | docs: /customization/changelog/ (fetched 2026-04-16) | Wired (git backend). |
| changelog.title | changelog | Pro | portable | niche | implemented | — | docs: /customization/changelog/ | v2.12+. |
| changelog.divider | changelog | Pro | portable | niche | implemented | — | docs: /customization/changelog/ | Wired. |
| changelog.filters.include / exclude | changelog | OSS | portable | required | implemented | — | internal/pipe/changelog/changelog.go | Regex. |
| changelog.groups[].title / regexp / order | changelog | OSS | portable | required | implemented | — | internal/pipe/changelog/changelog.go | Wired. |
| changelog.groups[].groups[] (subgroups) | changelog | Pro | portable | niche | implemented | — | docs: /customization/changelog/ | Single-level nested. |
| changelog.ai.use / model / prompt | changelog | Pro | portable | niche | implemented | — | docs: /customization/changelog/ | Anthropic / OpenAI / Ollama backends wired in `stage-changelog`. |

### 2.6 Signing

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| signs[] (generic) | sign | OSS | portable | required | implemented | — | internal/pipe/sign/sign.go | `SignConfig` + `stage-sign/src/lib.rs`. |
| signs[].cmd (gpg default, cosign) | sign | OSS | portable | required | implemented | — | internal/pipe/sign/sign.go | Subprocess. |
| signs[].signature template | sign | OSS | portable | required | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].args templated | sign | OSS | portable | required | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].artifacts (none/all/checksum/source/package/installer/diskimage/archive/sbom/binary) | sign | OSS | portable | required | partial | — | internal/pipe/sign/sign.go | All enum values wired (Session G). **Divergence**: when `artifacts: all`, anodizer includes `Signature` + `Certificate` artifacts (stage-sign/src/tests.rs:113-114 explicit assertion) — GR fix 87a55ea (2026-03-30, #6509) **excluded** them to break the recursive sign-then-checksum loop where `.sig` files were checksummed and re-signed. Anodizer must follow upstream. Verified 2026-05-07. |
| signs[].ids | sign | OSS | portable | strongly-suggested | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].if | sign | Pro | portable | strongly-suggested | implemented | — | docs: /customization/sign/ (fetched 2026-04-16) | Templated conditional. |
| signs[].stdin / stdin_file | sign | OSS | portable | strongly-suggested | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].certificate (cosign/rekor) | sign | OSS | portable | strongly-suggested | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].env | sign | OSS | portable | required | implemented | — | internal/pipe/sign/sign.go | Wired. |
| signs[].output | sign | OSS | portable | niche | implemented | — | internal/pipe/sign/sign.go | v2.13+, wired. |
| docker_signs[] | sign | OSS | portable | strongly-suggested | implemented | — | internal/pipe/sign/sign_docker.go | `DockerSignConfig` + `DockerSignStage`. |
| binary_signs[] (deprecated) | sign | OSS | portable | strongly-suggested | implemented | — | internal/pipe/sign/sign_binary.go | `BinarySignStage` wired in `commands/build.rs`. |

### 2.7 Docker

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| dockers[] (v1, deprecated in v2.12+) | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go | `DockerConfig` — legacy single-arch path. |
| docker.image_templates | docker | OSS | portable | required | implemented | — | internal/pipe/docker/docker.go | Templated. |
| docker.dockerfile | docker | OSS | portable | required | implemented | — | internal/pipe/docker/docker.go | Wired. |
| docker.templated_dockerfile | docker | Pro | portable | niche | implemented | — | docs: /customization/docker/ (fetched 2026-04-16) | Wired. |
| docker.extra_files | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go | Wired. |
| docker.templated_extra_files | docker | Pro | portable | niche | implemented | — | docs: /customization/docker/ | Wired. |
| docker.use (docker/buildx/podman) | docker | OSS | portable | strongly-suggested | implemented (superset) | — | internal/pipe/docker/ | Wired. **`podman` is an anodizer superset** — GoReleaser's OSS validator rejects `use: podman` with "invalid use: podman, valid options are [buildx docker]" (upstream docker_test.go:1501). Anodizer accepts it as a first-class backend for rootless CI contexts. |
| docker.build_flag_templates | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go | Wired. |
| docker.skip_build | docker | Pro | portable | niche | implemented | — | docs: /customization/docker/ | Wired. |
| docker.skip_push (bool / auto) | docker | OSS | portable | required | implemented | — | internal/pipe/docker/docker.go | `SkipPushConfig`. |
| docker.push_flags | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go | Wired. |
| docker.retry (attempts / delay / max_delay) | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go | `DockerRetryConfig`. **Deprecated upstream since v2.15.3 (commit a176567, doc: deprecations.md)** in favor of project-level `retry:`. Same applies to `docker_manifests.retry` and `dockers_v2.retry`. Anodizer accepts both shapes today. See §2.18 for the new global `retry:` row. Verified 2026-05-07. |
| docker_v2 pipe | docker | OSS | portable | required | implemented | — | internal/pipe/docker/v2/ | `DockerV2Config` — `stage-docker` v2 path. V2 retry predicate (`is_retriable_error_v2`) deliberately narrow — only `"manifest verification failed for digest"` retries, matching upstream v2/docker.go:544-549. Reviewed 2026-04-18. |
| docker_v2.platforms (multi-arch) | docker | OSS | portable | required | implemented | — | internal/pipe/docker/v2/ | Wired. |
| docker_v2.sbom (inline SBOM) | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/v2/ | Wired. |
| docker_v2.labels / annotations | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/v2/ | Wired. |
| docker_v2.build_args | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/v2/ | Wired. |
| docker_manifests | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/manifest.go | `DockerManifestConfig` — `stage-docker`. |
| dockerdigest | docker | OSS | portable | niche | implemented | — | internal/pipe/dockerdigest/digest.go | Digest pinning after push; wired. |
| dockerhub (description sync) | docker | Pro | portable | niche | implemented | — | docs: /customization/dockerhub/ (fetched 2026-04-16) | `DockerHubConfig` — `stage-publish/src/dockerhub.rs`. |

### 2.8 Linux Packages (nFPM)

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| nfpms[] | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | `stage-nfpm/src/lib.rs`. |
| nfpm.id / ids / package_name / file_name_template | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/nfpm.go | Wired. |
| nfpm.if | publish-nfpm | Pro | portable | strongly-suggested | implemented | — | docs: /customization/nfpm/ (fetched 2026-04-16) | `NfpmConfig.if_condition` #[serde(rename="if")] (config.rs:2980); gate in stage-nfpm/src/lib.rs:983. Verified 2026-04-18. |
| nfpm.vendor / homepage / maintainer / description / license | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.formats: deb | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.formats: rpm | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.formats: apk | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | Alpine-specific but wired. |
| nfpm.formats: termux.deb | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.formats: archlinux | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.formats: ipk | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | v2.1+, wired. |
| nfpm.umask / bindir / libdirs | publish-nfpm | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.epoch / prerelease / version_metadata / release / section / priority | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired + auto from semver. |
| nfpm.meta / changelog / goamd64 / mtime | publish-nfpm | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nfpm/ | Wired; goamd64 mapped to target triples. |
| nfpm.dependencies / provides / recommends / suggests / conflicts / replaces | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.contents[] (type: config / config\|noreplace / symlink / tree / ghost / dir) | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | `NfpmContent` full enum. |
| nfpm.contents[].file_info | publish-nfpm | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nfpm/ | mode/mtime/owner/group templated. |
| nfpm.templated_contents | publish-nfpm | Pro | portable | niche | implemented | — | docs: /customization/nfpm/ | `NfpmConfig.templated_contents` (config.rs:2986). Verified 2026-04-18. |
| nfpm.scripts (preinstall/postinstall/preremove/postremove) | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm.templated_scripts | publish-nfpm | Pro | portable | niche | implemented | — | docs: /customization/nfpm/ | `NfpmConfig.templated_scripts` (config.rs:2991). Verified 2026-04-18. |
| nfpm.rpm.* (summary/group/packager/buildhost/compression/prefixes/scripts/signature) | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | `NfpmRpmConfig`. |
| nfpm.deb.* (triggers/lintian_overrides/compression/signature/fields/breaks/predepends) | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | `NfpmDebConfig`. |
| nfpm.apk.* (scripts/signature) | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | `NfpmApkConfig`. |
| nfpm.archlinux.* | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | `NfpmArchlinuxConfig`. |
| nfpm.ipk.* (abi_version/alternatives/auto_install/essential/fields/predepends/tags) | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/ | `NfpmIpkConfig`. |
| nfpm.overrides (per-format) | publish-nfpm | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nfpm/ | Wired. |
| nfpm ConventionalFileName (per-packager shape) | publish-nfpm | OSS | portable | required | implemented | — | internal/pipe/nfpm/ | `stage-nfpm/src/filename.rs` (2026-04-16 closure). |
| nfpm passphrase env (NFPM_*_PASSPHRASE) | publish-nfpm | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nfpm/ | Priority order wired. |
| srpm | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/srpm/ | `stage-srpm` via rpmbuild subprocess. |

### 2.9 Publish — Homebrew / Cask

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| brews[] / homebrew_formulas[] | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | `HomebrewConfig` + `stage-publish/src/homebrew.rs`. |
| homebrew.name / alternative_names | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | Wired (`alternative_names` Pro). |
| homebrew.ids / goarm / goamd64 | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | Wired — `goarm=6` matches GoReleaser default. |
| homebrew.url_template / url_headers / download_strategy / custom_require / custom_block | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.homepage / description / license / caveats | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.install / extra_install / post_install / test | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.dependencies (name/os/type/version) | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.conflicts | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.service / plist | publish-homebrew | OSS | portable | niche | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.commit_msg_template / directory | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.skip_upload | publish-homebrew | OSS | portable | strongly-suggested | implemented (superset) | — | internal/pipe/brew/brew.go | Wired. **Anodizer additionally logs a warning on unrecognised `skip_upload` values** (not just `"true"` / `"auto"`) to catch typos early — GoReleaser accepts silently. Benign divergence. |
| homebrew.repository.* | publish-homebrew | OSS | portable | required | implemented | — | internal/pipe/brew/brew.go | `RepositoryConfig`. |
| homebrew.repository.pull_request.* | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | PR-based tap updates. |
| homebrew.repository.pull_request.check_boxes | publish-homebrew | Pro | portable | niche | implemented | — | docs: /customization/homebrew/ (fetched 2026-04-16) | Pro-only. |
| homebrew.repository.git (url/private_key/ssh_command) | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | Git-over-SSH fallback. |
| homebrew.commit_author.* | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/brew/brew.go | Wired. |
| homebrew.commit_author.signing | publish-homebrew | OSS | portable | niche | implemented | — | internal/pipe/brew/brew.go | v2.11+. |
| homebrew.app (DMG app) | publish-homebrew | Pro | portable | niche | implemented | — | docs: /customization/homebrew/ | Wired. |
| homebrew_casks[] | publish-homebrew | OSS | portable | strongly-suggested | partial | — | internal/pipe/cask/cask.go | `HomebrewCaskConfig` wired. **Per-arch block field order diverges from upstream**: anodizer emits `url` then `sha256` (`stage-publish/src/homebrew/cask.rs:24-25`), GR fix 87b542b (2026-04-16) flipped to `sha256` then `url` for golden-file alignment. Behavioral parity (the cask still installs); cosmetic divergence breaks downstream golden-file tests. Verified 2026-05-07. |
| homebrew_casks.binaries / app / manpages | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/cask/cask.go | Wired. |
| homebrew_casks.completions (bash/zsh/fish) | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/cask/cask.go | Wired. |
| homebrew_casks.generate_completions_from_executable | publish-homebrew | OSS | portable | niche | partial | — | internal/pipe/cask/cask.go | Config field present (`HomebrewCaskGeneratedCompletions` in core/src/config/publishers/homebrew.rs); cask template (`stage-publish/src/homebrew/cask.rs`) does NOT render the directive — parsed-but-ignored. GR cask.rb template emits it after `postflight` (cask.rb fix bb9062f, 2026-05-01); anodizer must add. Verified 2026-05-07. |
| homebrew_casks.url.* (template/verified/using/cookies/referer/headers/user_agent/data) | publish-homebrew | OSS | portable | strongly-suggested | implemented | — | internal/pipe/cask/cask.go | Wired. |
| homebrew_casks.hooks (v2.13+) | publish-homebrew | OSS | portable | niche | implemented | — | internal/pipe/cask/cask.go | Wired. |
| homebrew_casks.service / zap / uninstall | publish-homebrew | OSS | portable | niche | implemented | — | internal/pipe/cask/cask.go | Wired. |

### 2.10 Publish — Scoop / Chocolatey / Winget (Windows)

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| scoops[] (incl use: archive/msi/nsis) | publish-scoop | OSS | portable | strongly-suggested | implemented | — | internal/pipe/scoop/ | `ScoopConfig` + `stage-publish/src/scoop.rs`. |
| scoop.persist / pre_install / post_install / depends / shortcuts | publish-scoop | OSS | portable | strongly-suggested | implemented | — | internal/pipe/scoop/ | Wired. |
| scoop.repository.* | publish-scoop | OSS | portable | strongly-suggested | implemented | — | internal/pipe/scoop/ | Wired. |
| chocolateys[] | publish-chocolatey | OSS | portable | niche | implemented | — | internal/pipe/chocolatey/ | `ChocolateyConfig` + `stage-publish/src/chocolatey.rs`. |
| chocolatey.package_source_url / title / authors / project_url / use | publish-chocolatey | OSS | portable | niche | implemented | — | internal/pipe/chocolatey/ | Wired. |
| chocolatey.dependencies / api_key / source_repo | publish-chocolatey | OSS | portable | niche | implemented | — | internal/pipe/chocolatey/ | Wired. |
| chocolatey.require_license_acceptance / license_url / release_notes / summary / description / tags | publish-chocolatey | OSS | portable | niche | implemented | — | internal/pipe/chocolatey/ | Wired. |
| chocolatey.skip_publish / icon_url / copyright / project_source_url / docs_url / bug_tracker_url | publish-chocolatey | OSS | portable | niche | implemented | — | internal/pipe/chocolatey/ | Wired. |
| wingets[] | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | `WingetConfig` + `stage-publish/src/winget.rs`. |
| winget.publisher / publisher_url / publisher_support_url / privacy_url | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | Wired. |
| winget.package_identifier / package_name / use / product_code / url_template | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | Wired. |
| winget.path / homepage / description / license_url / copyright / copyright_url / release_notes / installation_notes | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | Wired. |
| winget.dependencies / tags / skip_upload | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | Wired. |
| winget.repository.* / commit_author.* | publish-winget | OSS | portable | strongly-suggested | implemented | — | internal/pipe/winget/ | Wired. |

### 2.11 Publish — AUR / Krew / Nix

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| aurs[] | publish-aur | OSS | portable | strongly-suggested | implemented | — | internal/pipe/aur/ | `AurConfig` + `stage-publish/src/aur.rs`. |
| aur.name / ids / homepage / description / maintainers / contributors / license / private_key / git_url | publish-aur | OSS | portable | strongly-suggested | implemented | — | internal/pipe/aur/ | Wired. |
| aur.skip_upload / provides / conflicts / depends / optdepends / backup / package / install / commit_msg_template | publish-aur | OSS | portable | strongly-suggested | implemented | — | internal/pipe/aur/ | Wired. |
| aur.goamd64 / git_ssh_command / url_template / directory / disable | publish-aur | OSS | portable | strongly-suggested | implemented | — | internal/pipe/aur/ | Wired. |
| aur_sources[] | publish-aur | OSS | portable | niche | implemented | — | internal/pipe/aursources/ | `AurSourceConfig` + `stage-publish/src/aur_source.rs`. |
| krews[] | publish-krew | OSS | portable | niche | implemented | — | internal/pipe/krew/ | `KrewConfig` + `stage-publish/src/krew.rs`; kubectl plugins. |
| krew.ids / goarm / goamd64 / url_template / commit_msg_template / homepage / description / short_description / caveats / skip_upload | publish-krew | OSS | portable | niche | implemented | — | internal/pipe/krew/ | Wired. |
| nix[] | publish-nix | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nix/ | `NixConfig` + `stage-publish/src/nix.rs`. |
| nix.name / ids / goamd64 / url_template / commit_msg_template / path / homepage / description / license / skip_upload | publish-nix | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nix/ | Wired. |
| nix.dependencies / install / extra_install / post_install / formatter | publish-nix | OSS | portable | strongly-suggested | implemented | — | internal/pipe/nix/ | Wired (ELF parser for architecture detection). |

### 2.12 Publish — Misc / Custom / Cloud / Pro

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| publishers[] (custom) | publish-custom | OSS | portable | strongly-suggested | implemented | — | internal/pipe/custompublishers/ | `PublisherConfig` + `publisher.rs`. |
| publishers.cmd / dir / ids / if / checksum / meta / signature / env / disable / extra_files / templated_extra_files / output | publish-custom | OSS | portable | strongly-suggested | implemented | — | internal/pipe/custompublishers/ | `if` / `templated_extra_files` / `output` (v2.11+) all wired. |
| artifactory (HTTP PUT) | publish-artifactory | OSS | portable | niche | implemented | — | internal/pipe/artifactory/ | `ArtifactoryConfig` + `stage-publish/src/artifactory.rs`. |
| artifactory.target / mode / username / password / client_x509_cert / client_x509_key / trusted_certificates | publish-artifactory | OSS | portable | niche | implemented | — | internal/pipe/artifactory/ | Wired. |
| artifactory.ids / exts / matrix (Pro) / custom_artifact_name / custom_headers / checksum / meta / signature / skip | publish-artifactory | OSS | portable | niche | implemented | — | internal/pipe/artifactory/ | `matrix` Pro-only; wired. |
| artifactory.extra_files / extra_files_only / templated_extra_files | publish-artifactory | OSS | portable | niche | implemented | — | internal/pipe/artifactory/ | Wired. |
| uploads[] (generic HTTP) | publish-custom | OSS | portable | niche | implemented | — | internal/pipe/upload/ | `UploadConfig` + `stage-publish/src/upload.rs`. |
| fury (fury.io apt/yum) | publish-fury | Pro | portable | niche | implemented | — | docs: /customization/fury/ (fetched 2026-04-16) | Proxy-rebranded; see `PublisherConfig`. |
| cloudsmith (apt/yum repo) | publish-cloudsmith | Pro | portable | niche | implemented | — | docs: /customization/cloudsmith/ (fetched 2026-04-16) | `CloudSmithConfig`. |
| npm (Pro) | publish-npm | Pro | not-applicable | not-applicable | n-a | — | docs: /customization/npm/ (fetched 2026-04-16) | JS runtime publish; no canonical Rust analogue. See §5. |
| crates.io publish | publish-cratesio | OSS | rust-additive | required | implemented | — | — | `CratesPublishConfig` + `stage-publish/src/crates_io.rs`; GoReleaser has no equivalent. |
| blob (s3/gs/azblob) | blob | OSS | portable | strongly-suggested | implemented | — | internal/pipe/blob/ | `BlobConfig` + `stage-blob/src/lib.rs`; parallel across configs. |
| blob.provider / bucket / endpoint / region / disable_ssl / ids / if / disable / directory / s3_force_path_style / acl / cache_control / content_disposition / include_meta | blob | OSS | portable | strongly-suggested | implemented | — | internal/pipe/blob/ | Wired. |
| blob KMS | blob | OSS | portable | niche | implemented | — | internal/pipe/blob/ | CLI-shell via aws/gcloud/az; intentional divergence from gocloud.dev. |
| blob.extra_files / extra_files_only / templated_extra_files | blob | OSS | portable | niche | implemented | — | internal/pipe/blob/ | Wired. |

### 2.13 Announce providers

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| announce.discord | announce-discord | OSS | portable | strongly-suggested | implemented | — | internal/pipe/discord/ | `stage-announce/src/discord.rs`. |
| announce.slack (channel/blocks/attachments) | announce-slack | OSS | portable | strongly-suggested | implemented | — | internal/pipe/slack/ | `stage-announce/src/slack.rs`. |
| announce.telegram (parse_mode MarkdownV2/HTML + message_thread_id) | announce-telegram | OSS | portable | niche | implemented | — | internal/pipe/telegram/ | `stage-announce/src/telegram.rs`. |
| announce.teams (AdaptiveCard) | announce-teams | OSS | portable | niche | implemented | — | internal/pipe/teams/ | `stage-announce/src/teams.rs`; uses AdaptiveCard (intentional divergence). |
| announce.mattermost | announce-mattermost | OSS | portable | niche | implemented | — | internal/pipe/mattermost/ | `stage-announce/src/mattermost.rs`. |
| announce.webhook | announce-webhook | OSS | portable | strongly-suggested | implemented | — | internal/pipe/webhook/ | `stage-announce/src/webhook.rs`; custom HTTP broadcast. |
| announce.smtp (email) | announce-smtp | OSS | portable | niche | implemented | — | internal/pipe/smtp/ | `stage-announce/src/email.rs`. |
| announce.reddit | announce-reddit | OSS | portable | niche | implemented | — | internal/pipe/reddit/ | `stage-announce/src/reddit.rs`. |
| announce.twitter | announce-twitter | OSS | portable | niche | implemented | — | internal/pipe/twitter/ | `stage-announce/src/twitter.rs`. |
| announce.mastodon (form-encoded POST) | announce-mastodon | OSS | portable | niche | implemented | — | internal/pipe/mastodon/ | `stage-announce/src/mastodon.rs`. |
| announce.bluesky (AT Proto) | announce-bluesky | OSS | portable | niche | implemented | — | internal/pipe/bluesky/ | `stage-announce/src/bluesky.rs`. |
| announce.linkedin | announce-linkedin | OSS | portable | niche | implemented | — | internal/pipe/linkedin/ | `stage-announce/src/linkedin.rs`. |
| announce.opencollective | announce-opencollective | OSS | portable | niche | implemented | — | internal/pipe/opencollective/ | `stage-announce/src/opencollective.rs`. |
| announce.discourse | announce-discourse | OSS | portable | niche | implemented | — | internal/pipe/discourse/ | `stage-announce/src/discourse.rs`. |

### 2.14 SBOM / Notarize / Snapcraft / Flatpak / Installers

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| sboms[] (syft) | sbom | OSS | portable | required | implemented | — | internal/pipe/sbom/ | `SbomConfig` + `stage-sbom`. |
| sbom.cmd / args / env / artifacts / ids / disable / documents | sbom | OSS | portable | required | implemented | — | internal/pipe/sbom/ | Wired; templates `${artifact}` / `${document}` / `${artifactID}`. |
| snapcrafts[] | publish-snap | OSS | portable | niche | implemented | — | internal/pipe/snapcraft/ | `SnapcraftConfig` + `stage-snapcraft`. |
| snap fields (grade/confinement/base/plugs/slots/layout/apps/assumes/hooks/extra_files) | publish-snap | OSS | portable | niche | implemented | — | internal/pipe/snapcraft/ | Wired. |
| flatpaks[] | publish-flatpak | OSS | portable | niche | implemented | — | internal/pipe/flatpak/ | `FlatpakConfig` + `stage-flatpak`. |
| flatpak fields (app_id/runtime/sdk/command/finish_args) | publish-flatpak | OSS | portable | niche | implemented | — | internal/pipe/flatpak/ | Wired. |
| dmgs[] (macOS disk image) | dmg | Pro | portable | strongly-suggested | implemented | — | docs: /customization/dmg/ (fetched 2026-04-16) | `DmgConfig.if_condition` (config.rs:3521); gate in stage-dmg/src/lib.rs:156. Verified 2026-04-18. **`dmgs.use:` accepts only `binary` or `appbundle`** (not `archive`) — intentional narrowness: anodizer expects either a raw built binary or a bundled .app, not an already-archived tarball. Users who need DMG from archive contents should extract into an appbundle first. |
| msis[] (Wix/wixl) | msi | Pro | portable | strongly-suggested | implemented (superset) | — | docs: /customization/msi/ (fetched 2026-04-16) | `MsiConfig.if_condition` + `.hooks: BuildHooksConfig` (config.rs:3555,3560); gate in stage-msi/src/lib.rs:291. `extra_files: Vec<String>` matches docs (WiX context filenames only). `goamd64` is Go-specific (n-a for Rust target triples). **Behavioral superset**: in v3 mode `extensions` are passed to BOTH `candle` and `light` (upstream docs pass only to `candle`) to avoid link-time ExtensionRequired errors from transform-bearing extensions. Verified 2026-04-18. |
| pkgs[] (macOS .pkg) | pkg | Pro | portable | strongly-suggested | implemented | — | docs: /customization/pkg/ (fetched 2026-04-16) | `PkgConfig.if_condition` (config.rs:3597); gate in stage-pkg/src/lib.rs:116. Verified 2026-04-18. |
| nsis[] (Windows installer) | nsis | Pro | portable | strongly-suggested | implemented | — | docs: /customization/nsis/ (fetched 2026-04-16) | `NsisConfig.if_condition` (config.rs:3631); gate in stage-nsis/src/lib.rs:124. `goamd64` is Go-specific (n-a for Rust target triples). Verified 2026-04-18. |
| app_bundles[] (macOS .app) | appbundle | Pro | portable | strongly-suggested | implemented | — | docs: /customization/app_bundles/ (fetched 2026-04-16) | `AppBundleConfig.if_condition` (config.rs:3667); gate in stage-appbundle/src/lib.rs:263. Verified 2026-04-18. |
| makeselfs[] (Linux self-extracting) | makeself | OSS | portable | niche | implemented | — | internal/pipe/makeself/ | `MakeselfConfig` + `stage-makeself`. |
| notarize.macos (anchore/quill) | notarize | OSS | portable | strongly-suggested | implemented | — | internal/pipe/notary/ | `NotarizeConfig` cross-platform backend. |
| notarize.macos_native (codesign/xcrun) | notarize | Pro | portable | strongly-suggested | implemented | — | docs: /customization/notarize/ (fetched 2026-04-16) | `MacOSNativeSignNotarizeConfig`. |
| ko (container builder from Go source) | docker | OSS | go-specific | not-applicable | n-a | — | internal/pipe/ko/ | Go-source-to-container; docker+docker_v2 covers Rust case. |

### 2.15 Project / Global / CLI / Partial

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| project_name | metadata | OSS | portable | required | implemented | — | internal/pipe/project/ | `Config.project_name`. |
| dist | metadata | OSS | portable | required | implemented | — | internal/pipe/project/ | `Config.dist`. |
| git.tag_sort (-version:refname / semver / smartsemver Pro) | misc | OSS | portable | required | implemented | — | internal/pipe/git/ | `GitConfig.tag_sort`. |
| git.prerelease_suffix | misc | OSS | portable | strongly-suggested | implemented | — | internal/pipe/git/ | Wired. |
| git.ignore_tags / ignore_tag_prefixes (Pro) | misc | OSS | portable | strongly-suggested | implemented | — | internal/pipe/git/ | Wired in tag discovery (2026-04-16). |
| monorepo.tag_prefix / dir | misc | Pro | portable | strongly-suggested | implemented | — | docs: /customization/monorepo/ (fetched 2026-04-16) | `MonorepoConfig` + `WorkspaceConfig` native. |
| includes[].from_file / from_url | misc | Pro | portable | niche | implemented | — | docs: /customization/includes/ (fetched 2026-04-16) | `IncludeSpec` enum. |
| metadata.mod_timestamp / maintainers / license / homepage / description | metadata | Pro | portable | strongly-suggested | implemented | — | docs: /customization/metadata/ (fetched 2026-04-16) | `MetadataConfig` with `full_description: ContentSource` + `commit_author: CommitAuthorConfig` (config.rs:4497,4501). Verified 2026-04-18. |
| metadata.full_description.from_url | metadata | Pro | portable | niche | partial | — | docs: /customization/metadata/ (fetched 2026-04-16) | `ContentSource::FromUrl` variant parses, but core/src/context.rs:754 errors "`from_url` is not yet supported at metadata context time". Inline + FromFile work. |
| mcp registry (MCP server manifest publish) | publish-mcp | OSS | portable | niche | missing | — | internal/pipe/mcp/mcp.go | New 2026-03+ pipe; publishes server-manifest JSON to MCP registry (`/v0/publish`). v2.16+ now wraps the publish call in `retryx.Do` against `Project.Retry`. Auth methods: `none`, `github`, `github-oidc`. Schema: `name`, `title`, `description`, `homepage`, `auth.{type,token}`, `repository.{url,source,id,subfolder}`, `packages[].{registry_type,identifier,transport.type}`, `disable`. Rust MCP servers exist (rmcp); reclassify-candidate for `strongly-suggested` once Rust MCP ecosystem matures. |
| env (global env list) | misc | OSS | portable | required | implemented | — | internal/pipe/env/ | Wired. |
| env_files.github_token / gitlab_token / gitea_token | misc | OSS | portable | strongly-suggested | implemented | — | internal/pipe/env/ | `EnvFilesConfig`. |
| template_files[] | misc | Pro | portable | niche | implemented | — | docs: /customization/template_files/ (fetched 2026-04-16) | `TemplateFileConfig` + `stage-templatefiles`. |
| before / after hooks | hooks | OSS | portable | required | implemented | — | internal/pipe/before/ | `HooksConfig` + `hooks.rs`. |
| snapshot.name_template | misc | OSS | portable | required | implemented | — | internal/pipe/snapshot/ | `SnapshotConfig`. |
| nightly (Pro) | misc | Pro | portable | niche | implemented | — | docs: /pro/ (fetched 2026-04-16) | `NightlyConfig`. |
| partial builds (--split/--merge) | partial | Pro | portable | strongly-suggested | implemented | — | internal/pipe/partial/ | `PartialConfig` + `commands/continue_cmd.rs`. |
| Split/merge GGOOS/GGOARCH | partial | Pro | go-specific | not-applicable | n-a | — | internal/pipe/partial/ | Rust-native replacement: `ANODIZER_SPLIT_TARGET`. |
| CLI: release | cli | OSS | portable | required | implemented | — | cmd/release.go | `commands/release/mod.rs`. |
| CLI: build | cli | OSS | portable | required | implemented | — | cmd/build.go | `commands/build.rs` (build parity 2026-04-16). |
| CLI: check | cli | OSS | portable | required | implemented | — | cmd/check.go | `commands/check.rs`. |
| CLI: healthcheck | cli | OSS | portable | strongly-suggested | implemented | — | cmd/healthcheck.go | `commands/healthcheck.rs`. |
| CLI: init | cli | OSS | portable | strongly-suggested | implemented | — | cmd/init.go | `commands/init.rs`. |
| CLI: completion | cli | OSS | portable | strongly-suggested | implemented | — | cmd/completion.go | `commands/completion.rs`. |
| CLI: jsonschema | cli | OSS | portable | niche | implemented | — | cmd/jsonschema.go | `commands/jsonschema.rs`. |
| CLI: changelog preview | cli | Pro | portable | niche | implemented | — | docs: /pro/ (fetched 2026-04-16) | `commands/changelog.rs`. |
| CLI: continue / publish / announce (--merge) | cli | Pro | portable | strongly-suggested | implemented | — | cmd/continue.go | `commands/continue_cmd.rs` + `publish_cmd.rs` + `announce_cmd.rs`. |
| CLI: man pages | cli | OSS | portable | niche | missing | — | cmd/mangen.go | `goreleaser man` generates man pages; anodizer has no `man` subcommand. |
| Flag: --auto-snapshot | cli | OSS | portable | strongly-suggested | implemented | — | cmd/release.go | Wired. |
| Flag: --clean | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired. |
| Flag: --config | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired. |
| Flag: --draft | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired. |
| Flag: --fail-fast | cli | OSS | portable | strongly-suggested | implemented | — | cmd/release.go | Wired. |
| Flag: --id (Pro) | cli | Pro | portable | strongly-suggested | implemented | — | cmd/release.go | `--crate` filter. |
| Flag: --key (Pro license) | cli | Pro | not-applicable | not-applicable | n-a | — | cmd/release.go | Pro licensing; anodizer is OSS, no analogue needed. |
| Flag: --nightly (Pro) | cli | Pro | portable | niche | implemented | — | cmd/release.go | Wired via `NightlyConfig`. |
| Flag: --parallelism | cli | OSS | portable | strongly-suggested | implemented | — | cmd/release.go | Bounded concurrency across stages. |
| Flag: --prepare (Pro) | cli | Pro | portable | strongly-suggested | implemented | — | cmd/release.go | `prepare: bool` flag on `ReleaseOpts` (commands/release/mod.rs:48); `apply_prepare_mode_to_skip` adds release/publish/announce to skip list. Verified 2026-04-18. |
| Flag: --release-notes / --release-notes-tmpl | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired. |
| Flag: --skip | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired with `--skip=unknown` parse-time error. |
| Flag: --snapshot | cli | OSS | portable | required | implemented | — | cmd/release.go | Wired. |
| Flag: --split (Pro) | cli | Pro | portable | strongly-suggested | implemented | — | cmd/release.go | Wired via `PartialConfig`. |
| Flag: --timeout | cli | OSS | portable | strongly-suggested | implemented | — | cmd/release.go | Wired. |
| Flag: --single-target (build) | cli | OSS | portable | strongly-suggested | implemented | — | cmd/build.go | Wired. |
| Flag: --soft (Pro, check only) | cli | Pro | portable | niche | missing | — | cmd/check.go | Non-fatal validation mode; anodizer `check` is strict by default. |

### 2.16 Template helpers

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| Tera-native templating | template-helpers | — | rust-native-replacement | required | implemented | — | — | anodizer uses Tera; GoReleaser uses Go text/template. Pre-processor bridges Go syntax. |
| String helpers (replace/split/tolower/toupper/trim/trimprefix/trimsuffix/contains/title) | template-helpers | OSS | portable | required | implemented | — | internal/tmpl/ | Wired in `core/src/template.rs`. |
| Path helpers (dir/base/abs) | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | Wired. |
| Filter helpers (filter/reverseFilter) | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | Uses Rust `regex` not POSIX ERE — intentional. |
| Version helpers (incpatch/incminor/incmajor) | template-helpers | OSS | portable | required | implemented | — | internal/tmpl/ | Wired. |
| Env helpers (envOrDefault/isEnvSet) | template-helpers | OSS | portable | required | implemented | — | internal/tmpl/ | Wired. |
| Hash functions (blake2b/blake2s/blake3/crc32/md5/sha1/224/256/384/512/sha3_*) | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | Wired (v2.9+/v2.15+). |
| File I/O (readFile/mustReadFile) | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | v2.12+, wired. |
| Data structures (list/map/indexOrDefault) | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | Wired. |
| Encoding (mdv2escape/urlPathEscape) | template-helpers | OSS | portable | niche | implemented | — | internal/tmpl/ | Wired (v2.5+). |
| Misc (time/englishJoin) | template-helpers | OSS | portable | niche | implemented | — | internal/tmpl/ | `englishJoin` v2.14+. |
| Pro helpers (in / reReplaceAll) | template-helpers | Pro | portable | strongly-suggested | implemented | — | docs: /customization/templates/ | Wired (`in` / `reReplaceAll` v2.8+). |
| .Now.Format preprocessor | template-helpers | OSS | portable | strongly-suggested | implemented | — | internal/tmpl/ | Custom preprocessor rewrites to `Now \| now_format(format=)` for Tera. |
| Variables (.ProjectName/.Version/.Tag/.Major/.Minor/.Patch etc.) | template-helpers | OSS | portable | required | implemented | — | internal/tmpl/ | Full set wired in `core/src/template.rs`. |
| Pro variables (.PrefixedTag/.Artifacts/.Metadata/.Var/.IsMerging/.IsRelease) | template-helpers | Pro | portable | strongly-suggested | implemented | — | docs: /customization/templates/ | Wired. |
| Custom variables (.Var.*) | template-helpers | Pro | portable | niche | implemented | — | docs: /customization/templates/ | Via `variables` config. |

### 2.17 Other / Auth / Artifacts JSON

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| dist/artifacts.json | misc | OSS | portable | required | implemented | — | internal/artifact/ | Emitted end-of-pipeline. |
| dist/metadata.json | misc | OSS | portable | required | implemented | — | internal/pipe/metadata/ | Emitted end-of-pipeline. |
| dist/config.yaml (effective config) | misc | OSS | portable | strongly-suggested | implemented | — | internal/pipe/effectiveconfig/ | Emitted (build parity 2026-04-16). |
| GITHUB_TOKEN / GITLAB_TOKEN / GITEA_TOKEN | misc | OSS | portable | required | implemented | — | internal/client/ | Wired. |
| GORELEASER_FORCE_TOKEN | misc | OSS | portable | niche | implemented | — | internal/client/ | `ForceTokenKind` enum. |
| Announcer provider secret env vars | announce-* | OSS | portable | required | implemented | — | — | All listed env vars wired in their stage-announce modules. |
| continue-on-error | misc | OSS | portable | niche | missing | — | internal/pipe/ | GoReleaser permits stages to log-and-continue via `continue_on_error` on certain pipes (e.g. docker skip on cert error); anodizer default is fail-fast. |
| version_sync (Pro? — anodizer-additive) | misc | — | rust-additive | strongly-suggested | implemented | — | — | `VersionSyncConfig` enforces `Cargo.toml` ↔ tag alignment; GoReleaser has no equivalent. |
| binstall metadata | misc | — | rust-additive | strongly-suggested | implemented | — | — | `BinstallConfig` emits `cargo-binstall` hints in release metadata. |
| skip memento (operator-visible skip summary) | misc | — | rust-additive | strongly-suggested | implemented | — | — | `anodizer_core::pipe_skip::SkipMemento`; end-of-pipeline report of intentional skips. |

---

### 2.18 Refresh delta (2026-05-07)

Rows added / re-classified after walking 50 commits in `v2.15.0..HEAD` (HEAD `8976559`, tag `v2.16.0-17315a55-nightly-1-g8976559`). Each row cites both upstream source path and anodizer source/test path. Status reflects whether anodizer's behavior matches the **new** upstream behavior — pre-existing matching rows are not duplicated here.

#### 2.18.1 New OSS features

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| `retry:` (project-level global) | misc | OSS | portable | required | missing | — | internal/retryx/retryx.go + pkg/config/config.go::Project.Retry | New v2.15.3 (#6528, commit a176567). Fields: `attempts` (default 10), `delay` (default 10s), `max_delay` (default 5m). Applied across **all** network ops: GitHub/GitLab/Gitea API, every announcer (Discord/Telegram/Slack/Mastodon/Teams/Reddit/Twitter/Bluesky/LinkedIn/Discourse/Mattermost/Webhook/OpenCollective/MCP), HTTP uploads (artifactory/custom/snapcraft store/gomod proxy), and docker push/manifest. Retriable: network errors, HTTP 5xx, HTTP 429. Anodizer has per-stage ad-hoc retry (stage-docker/src/retry.rs, stage-release/src/lib.rs::retry_upload, stage-publish/src/chocolatey/package.rs ad-hoc) but **no unified `Project.Retry`** struct nor cross-pipe wiring. Doc: /customization/general/retry/. |
| `archives.formats: xz` (single-file) | archive | OSS | portable | niche | missing | — | pkg/archive/xz/xz.go + pkg/archive/archive.go (commit bb532b6, v2.15+) | New single-file format parallel to `gz`. xz writer single-file constraint: returns "only one file can be archived in xz format" if multiple files added. anodizer supports compound `tar.xz`/`txz` only (`stage-archive/src/formats.rs`); plain `xz` not in `formats:` enum (`core/src/config/archives.rs:list`). |
| builder: node (Node.js SEA) | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/node/build.go (commit 9500ead, 2026-05-03) | Node.js Single-Executable-Application builder: shells out to `node ≥v25.5 --build-sea sea-config.json`, fetches per-target Node binary from nodejs.org/dist (verifies SHA-256), ad-hoc-signs Mach-O via quill on darwin. Inputs: `package.json`, JS entrypoint. **n-a — anodize is a Rust binary builder; Node SEA is a JS-to-binary tool, no Rust analogue.** |
| build target syntax: `<triple>.<glibc-version>` (Rust) | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/rust/build.go (commits 634a0cb, e4262d5, 9ee7477) | User-pinnable glibc via target suffix, e.g. `x86_64-unknown-linux-gnu.2.38`. anodizer already strips the suffix for cargo invocation (`stage-build/src/validation.rs::strip_glibc_suffix` + tests). Verified parity 2026-05-07. |
| Rust targets: `arm-unknown-linux-musleabihf`, `armv7-unknown-linux-musleabihf` | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/rust/all_targets.txt (commit e35ff62) | Two musl-soft-float ARM targets added upstream. anodizer already lists both (`stage-build/src/targets.rs`). Verified 2026-05-07. |
| Rust ARM grouping: per-armv level archive grouping | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/rust/build.go (commit 03735a4, #6582) | Upstream now sets `Goarch=t.Arch` and `Goarm=t.Arm` so archives group by armv5/v6/v7 instead of merging under generic `arm`. anodizer's `core/src/target.rs::map_target` already returns `armv7`/`armv6` (Rust-style arch) directly; archive grouping uses target triple, so groups are correct by construction. |
| Rust workspace error message lists all members | build | OSS | portable | strongly-suggested | implemented | — | internal/builders/rust/build.go (commit 889107f) | Cosmetic: GR's error string now lists every workspace member, not just the first. anodizer's workspace error path bypasses this code (uses `cargo metadata` directly); the upstream behavior is informational not a divergence. |

#### 2.18.2 New behavioral fixes (rows where anodizer must update to track upstream)

| name | category | tier | scope | ecosystem_relevance | parity_status | disposition | source_ref | notes |
|------|----------|------|-------|---------------------|---------------|-------------|------------|-------|
| `release.publish` preserves prerelease/name/make_latest on un-draft PATCH | release | OSS | portable | required | partial | — | internal/client/github.go (commits 6ecba31 + 2e17678) | When converting draft → published, GR PATCH body now includes: `name` (from `name_template`), `prerelease: true` if `ctx.PreRelease`, and `make_latest: false` if prerelease. anodizer's PATCH body (`stage-release/src/github/mod.rs:655`) sends only `{draft:false}` plus optional `make_latest`/`discussion_category_name`; missing the `name` re-template and the auto `make_latest=false` for prereleases. GitHub PATCH preserves unset fields server-side, so `prerelease` and original `name` carry through, but the auto-`make_latest=false` for prereleases is missing. |
| GitHub author lookup via Search Users API removed | changelog | OSS | portable | required | partial (superset) | — | internal/client/github.go (commit 17315a5, #6601) | Upstream **removed** `c.client.Search.Users(...)` fallback for resolving author username from email. Now only the noreply pattern `ID+USERNAME@users.noreply.github.com` is honored; other emails get no `Username`. Reasons cited upstream: secondary rate-limit hits on big releases; private emails make lookup useless anyway. anodizer's `stage-release/src/github/username.rs:83` still calls `/search/users?q={}+in:email`. **Decide**: track upstream simplification (fewer API calls, no rate-limit hits) OR keep as superset (better UX for users with public emails). Recommend tracking upstream. |
| github prerelease `make_latest=false` default | release | OSS | portable | required | partial | — | internal/client/github.go (commit 6ecba31) | When `ctx.PreRelease == true`, GR forces `make_latest = "false"` regardless of user template. anodizer's stage-release passes through user-templated `make_latest` only — does not auto-suppress on prereleases. |
| `cask.rb` per-arch block: `sha256` precedes `url` | publish-homebrew | OSS | portable | strongly-suggested | partial | — | internal/pipe/cask/templates/{linux_packages,macos_packages}.rb (commit 87b542b) | See §2.9 row downgrade. anodizer template emits `url` first; align to upstream order (cosmetic but breaks downstream golden-file diffing). |
| `cask.rb` `generate_completions_from_executable` emitted after postflight | publish-homebrew | OSS | portable | niche | partial | — | internal/pipe/cask/templates/cask.rb (commit bb9062f, issue #5958) | See §2.9 row downgrade. anodizer's cask template doesn't render the field at all. Must add after postflight stanza so quarantine-removal in postflight runs before completion-generation. |
| sign `artifacts: all` excludes Signature + Certificate | sign | OSS | portable | required | partial | — | internal/pipe/sign/sign.go (commit 87a55ea, #6509) | See §2.6 row downgrade. anodizer explicitly includes them for parity with **old** GR behavior; new GR excludes to break recursion. |
| `checksums` excludes Signature + Certificate | checksum | OSS | portable | required | implemented | — | internal/pipe/checksums/checksums.go (commit b5eabc8) | anodizer's `stage-checksum/src/run.rs:572-578` skip set already includes `Signature` + `Certificate` + `Checksum`. Verified 2026-05-07. |
| `dockers/v2` Dockerfile-template emptiness check after rendering | docker | OSS | portable | required | partial | — | internal/pipe/docker/v2/docker.go (commit d788340) | GR now checks rendered Dockerfile (post-template) for emptiness, not the raw template. So `{{ if .IsSnapshot }}Dockerfile{{ end }}` correctly skips during release. anodizer must verify it renders before the empty-check; gated as partial pending grep. |
| `dockers/v2 parsePlatform` nil-safe on missing arch | docker | OSS | portable | strongly-suggested | partial | — | internal/pipe/docker/v2/docker.go (commit 9e9f87c) | GR allows platform string `"linux"` (no arch) without panic. anodizer must verify equivalent in `stage-docker/src/v2/`; gate as partial pending grep. |
| `dockers/v2` digest log fields split: `images` + `digest` | docker | OSS | portable | niche | partial | — | internal/pipe/docker/v2/docker.go (commit e7a4afa) | Cosmetic: GR previously logged `images` field with embedded `@digest` per line; now logs `images` and `digest` as separate fields. Affects observability tools matching log structure. |
| `dockers v1` healthcheck delegates to v2 when `use: buildx` | docker | OSS | portable | strongly-suggested | partial | — | internal/pipe/docker/docker.go (commit e09e23a, #6526) | GR now runs the `buildx version` probe through v1 docker pipe when `use: buildx` is configured, plus registers `docker.Pipe{}` in `DependencyCheckers`. anodizer's `stage-docker` healthcheck path covers this for v2 only — verify v1+buildx code path. |
| `dockers/v1` deprecated marker | docker | OSS | portable | strongly-suggested | implemented | — | internal/pipe/docker/docker.go (commit e09e23a) | GR's `Pipe` and `ManifestPipe` carry `// Deprecated: use docker v2.` comment; informational. anodizer recommends `docker_v2` in its README; no behavioral change. |
| docker manifest "did you mean?" suggests other image, not input | docker | OSS | portable | niche | implemented | — | internal/pipe/docker/manifest.go (commit 921e6cb) | GR fixed bug where suggestion echoed input back. anodizer's `stage-docker/src/run.rs:687` already filters `d > 0 && d <= img.len() / 2`, sidestepping the bug. Verified 2026-05-07. |
| `changelog.abbrev` panic-safe for negative values < -1 | changelog | OSS | portable | required | implemented (superset) | — | internal/pipe/changelog/changelog.go (commit 88daaf3) | GR now clamps `abbr = max(abbr, -1)`. anodizer's changelog uses Rust pattern-match — no panic on out-of-range; verify clamp parity. |
| `changelog.format` debug log uses %t for bool | changelog | OSS | portable | niche | implemented | — | internal/pipe/changelog/changelog.go (commit 6c7798f) | GR fixed `%b` → `%t`. Cosmetic; anodizer uses tracing `?match` debug — no Go-printf format, no divergence. |
| `checksums refreshAll` sort tolerates lines without double-space | checksum | OSS | portable | niche | partial | — | internal/pipe/checksums/checksums.go (commit f39c233) | GR's sort now uses `strings.Cut` instead of indexing `Split[1]`. anodizer's checksum sort path (`stage-checksum/src/run.rs`) uses Rust slicing — verify no panic on malformed lines. |
| `nfpm` content mtime parse error reports the bad value | publish-nfpm | OSS | portable | niche | implemented | — | internal/pipe/nfpm/nfpm.go (commit 50a034d) | Cosmetic error-message fix. anodizer's nfpm-content parse uses anyhow context — verify error includes the offending mtime. |
| `aur`/`aursources`/`krew` template expansion of `skip_upload` before bool check | publish-aur/krew | OSS | portable | strongly-suggested | partial | — | internal/pipe/aur/aur.go + aursources/aursources.go + krew/krew.go (commit cba5b9f) | GR now templates `skip_upload` so `{{ .IsSnapshot }}` works. anodizer must verify all three publishers run the value through the template engine before `bool/auto/empty` parsing. |
| `release` log uses correct repo (gitlab/gitea), not always github | release | OSS | portable | strongly-suggested | partial | — | internal/pipe/release/release.go (commit 44133de) | GR helper `releaseRepo(ctx)` switches on `ctx.TokenType`. anodizer must verify the publish log uses the matching `Release.GitLab` / `Release.Gitea` / `Release.GitHub` based on detected token. |
| `srpm` no double-close on package file | srpm | OSS | portable | niche | implemented | — | internal/pipe/srpm/srpm.go (commit 053c68a) | GR removed double Close. anodizer's `stage-srpm` writes via owned File — Drop closes once. |
| `blob` provider is templated before S3-ACL gate | blob | OSS | portable | niche | partial | — | internal/pipe/blob/upload.go (commit 4d1924d) | GR now applies tmpl to `conf.Provider` before `provider == "s3"` check. anodizer must verify provider value flows through `Tmpl::apply()` before stage routing. |
| `gitea` create-file falls back to server default branch (no hard-coded "master") | release | OSS | portable | niche | partial | — | internal/client/gitea.go (commit 4a9d25f) | GR now leaves branch empty so Gitea uses repo's default branch. anodizer's gitea client path (publishers + release) must verify same. |
| `bun` builder parse-error includes raw target | build | OSS | not-applicable | not-applicable | n-a | — | internal/builders/bun/targets.go (commit 2a10e3e) | Bun is a JS runtime builder — n-a (anodize is Rust). |
| `partial` adds `ppc64le → GGOPPC64/GOPPC64` env mapping | partial | OSS | go-specific | not-applicable | n-a | — | internal/pipe/partial/partial.go (commit e15276b) | Go-specific env-mapping table. anodizer uses target triples directly, no GGOOS/GGOARCH layer. n-a (rust-native-replacement). |
| `partial` mips64/mips64le use GGOMIPS64/GOMIPS64 | partial | OSS | go-specific | not-applicable | n-a | — | internal/pipe/partial/partial.go (commit a05ecb8) | Same as above — Go matrix axes. n-a. |
| `tmpl.filter` returns error instead of panicking on bad regex | template-helpers | OSS | portable | required | implemented (superset) | — | internal/tmpl/tmpl.go (commit c2f16b9) | GR's `filter`/`reverseFilter` now return `(string, error)` from `regexp.CompilePOSIX`. anodizer uses Rust `regex::Regex::new` returning `Result`, never panics. Already superset. |
| `redact.Writer` returns 0 bytes on underlying write failure | misc | OSS | portable | niche | partial | — | internal/redact/redact.go (commit f48613d) | GR's `redactWriter.Write` now returns `(0, err)` on inner-write failure (was `(len(p), err)`). anodizer's redaction layer must verify write-error bytes-written contract. |
| sec: removed env-vars + git-remote + http-auth + webhook-headers from logs; redact target/request URLs; redact length≥1 (was ≥10) | misc | OSS | portable | required | partial | — | commit d1cdbb2 | Anodizer must audit log statements in `stage-build`, `core/src/git`, `core/src/http_redact` (if exists), `stage-publish/src/util/http.rs`, and any webhook headers logging. Threshold change: redact every secret with length ≥ 1, not ≥ 10. Sweep target. |
| `build/base.Exec` log handles single-element command | build | OSS | portable | niche | implemented | — | internal/builders/base/build.go (commit ff02d82) | GR's panic on `command[1]` access for 1-element command (e.g. `["true"]`); now `strings.Join`. anodizer's `stage-build` invokes via Tokio `Command` — no array-index panic possible. |
| `winget` writes local YAML via filepath.Join (not path.Join) | publish-winget | OSS | portable | niche | implemented | — | internal/pipe/winget/template.go (commit ed201bd) | Windows path-sep correctness. anodizer uses `std::path::PathBuf::push` — already correct. |
| `mattermost` defaults `Color` from `Mattermost.Color` (not `Teams.Color`) | announce-mattermost | OSS | portable | niche | partial | — | internal/pipe/mattermost/mattermost.go (commit 7e7f9b2, #6533) | GR bug: mattermost default-Color and announce-Color both read from `Teams.Color`. Fixed to use `Mattermost.Color`. anodizer must verify mattermost stage reads its own struct's Color, not Teams'. |
| `linkedin` API responses parsed via typed structs | announce-linkedin | OSS | portable | niche | implemented (superset) | — | internal/pipe/linkedin/client.go (commit e31f01d) | GR replaced `map[string]any` + type-assertion (panic risk) with typed `profileResponse`/`shareResponse`. anodizer always parses via serde-typed structs — already superset. |
| `linkedin` error handling improvements | announce-linkedin | OSS | portable | niche | partial | — | internal/pipe/linkedin/client.go (commit 0944b0e) | Wrap-and-context error chain refactor. anodizer must mirror error categorisation (auth vs share vs profile-fetch). |
| `webhook` error handling improvements | announce-webhook | OSS | portable | niche | partial | — | internal/pipe/webhook/webhook.go (commit bba909e) | Mirror error wrapping. |
| `opencollective` handles mutation errors (was failing silently) | announce-opencollective | OSS | portable | niche | partial | — | internal/pipe/opencollective/opencollective.go (commit 206120a, #6512) | GR now reports GraphQL mutation errors (e.g. token lacks permission). anodizer must verify response-error parsing for mutation queries. |
| `snapcraft` retries upload on 5xx (hardcoded constants) | publish-snap | OSS | portable | niche | partial | — | internal/pipe/snapcraft/snapcraft.go (commit eb944f9, #6504) | GR removed user-configurable `Retry` from `config.Snapcraft` and hardcoded 10×10s exponential backoff for `snapcraft push` 5xx. **NB**: Now superseded by Project.Retry. anodizer's `stage-snapcraft` must wrap snapcraft push in retry. |
| `gomod proxy` 404 retry with backoff | misc | OSS | go-specific | not-applicable | n-a | — | internal/pipe/gomod/gomod_proxy.go (commit a176567) | Go module proxy fetch retry. n-a (rust-native-replacement: anodizer uses Cargo.lock fidelity). |
| Rate-limit checker: single check + ctx-aware sleep (no recursion) | release | OSS | portable | strongly-suggested | partial | — | internal/client/github.go (commit 60028b1) | GR's old recursive `rateLimitChecker` had no depth bound (stack overflow risk if rate-limit never recovered). Now: `checkRateLimit` does single-check, `time.After + ctx.Done()` select. anodizer's `stage-release/src/github/rate_limit.rs` must verify it's iterative (not recursive) and ctx-cancellable. |
| GitHub `updateRelease` nil-guards `resp` | release | OSS | portable | niche | partial | — | internal/client/github.go (commit 1ca21f0) | Defensive nil-check on `resp` before accessing `resp.Header.Get("X-GitHub-Request-Id")`. anodizer must check octocrab response handling for nil panic. |
| Gitea push log: removed duplicate `name` field | publish-* (gitea) | OSS | portable | niche | implemented | — | internal/client/gitea.go (commit 68ebdd7) | Cosmetic. |
| `git config` extraction preserves underlying error context | misc | OSS | portable | niche | partial | — | internal/git/config.go (commit 5042b84) | Wrap `err` instead of replacing with sentinel string. anodizer's `core/src/git/config.rs` must verify `?` propagation preserves underlying message. |
| `bodyOf` returns descriptive error on `io.ReadAll` failure | misc | OSS | portable | niche | partial | — | internal/client/* (commit 8b77358) | Was discarded; now wrapped. anodizer's HTTP body-reader paths should return errors, not silently truncate. |
| `sbom` artifact Name uses matched filename (not glob pattern) | sbom | OSS | portable | required | partial | — | internal/pipe/sbom/sbom.go (commit 292203e) | GR fixed `Name: filepath.Base(path)` → `Name: filepath.Base(match)`. Affects SBOM artifact names when `documents:` glob matches multiple files. anodizer's `stage-sbom` must verify it uses the matched filename, not the input glob. |
| `dockers/v2` no duplicate `WithOutput` in error wrapping | docker | OSS | portable | niche | implemented | — | internal/pipe/docker/v2/docker.go (commit a0875e5) | Cosmetic dedup. anodizer error wraps don't double-attach output. |
| `targz` package closes gzip reader in `Copy` | misc | OSS | portable | niche | implemented | — | pkg/archive/targz/targz.go (commit 0099417) | Resource cleanup. anodizer's `stage-archive/src/formats.rs` uses `?` + Drop; flate2 GzDecoder closes on drop. |
| `build` per-binary artifact IDs for `./...` builds | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go (commit 3140abb) | Go `./...` ellipsis support. n-a — Rust uses `--bin` selectors, no go-style ellipsis. |
| `build` allow explicit `binary` with ellipsis when single main resolves | build | OSS | go-specific | not-applicable | n-a | — | internal/builders/golang/build.go (commit d077fe1) | Same. n-a. |

#### 2.18.3 Action layer changes (rolled into separate file)

See `goreleaser-action-feature-inventory.md` §refresh-2026-05-07 for `version: nightly` immutable-tag resolution (action ≥ v7.2.0) and the new tag format `vX.Y.Z-<sha>-nightly`.


## 3. Rust-additive features (beyond GoReleaser)

Features anodizer added beyond what GoReleaser provides. Not parity gaps — these are dogfooding-matrix rows.

| name | category | source_ref | value |
|------|----------|------------|-------|
| crates.io publish (`publish.crates_io`) | publish-cratesio | `crates/stage-publish/src/crates_io.rs` | Native `cargo publish` integration, `Cargo.lock` fidelity, optional version gate. |
| cargo-binstall metadata | misc | `crates/stage-publish/src/binstall.rs` | Populates `binstall` block in `Cargo.toml` / emits install hints; GoReleaser has no equivalent. |
| Cargo-workspace awareness (tag → crate resolution) | misc | `crates/core/src/config.rs` `WorkspaceConfig` + `commands/resolve_tag.rs` | Workspace monorepo model native to Cargo. |
| Version_sync | misc | `crates/stage-build/src/version_sync.rs` | `Cargo.toml` version ↔ tag alignment check. |
| SkipMemento | misc | `crates/core/src/pipe_skip.rs` | Operator-visible intentional-skip summary at end of pipeline. |
| ConventionalFileName per-packager | publish-nfpm | `crates/stage-nfpm/src/filename.rs` | nfpm v2.44 per-format filename logic (deb/rpm/apk/archlinux/ipk/termux.deb). |
| Parallelism helper (`run_parallel_chunks`) | misc | `crates/core/src/parallel.rs` | Bounded concurrency + submission-order + fail-fast + panic attribution across 10 stages. |
| Retry for uploads (HTTP/publishers) | publish-* | (candidate, see known-bugs pre-seed) | GoReleaser does not retry for artifactory/fury/cloudsmith/custom; anodizer could reuse Docker V2's retry/backoff. |
| `targets --json` subcommand | cli | `crates/cli/src/commands/targets.rs` | JSON matrix for GH Actions `strategy.matrix`, used by anodizer-action. |
| `resolve-tag` subcommand | cli | `crates/cli/src/commands/resolve_tag.rs` | Tag → crate path resolution for monorepos. |
| ANODIZER_CURRENT_TAG / ANODIZER_PREVIOUS_TAG env | misc | `crates/core/src/git.rs` | Operator tag override that still runs upstream HEAD validation. |
| Tag hooks (`tag_pre_hooks` / `tag_post_hooks`) | hooks | `crates/cli/src/commands/tag.rs` | Tag-subcommand-scoped hooks with templated vars. |
| UPX target-triple glob | build | `crates/stage-upx/src/lib.rs` | Uses Rust target triples (more precise than goos/goarch). |

---

## 4. Permanent negative space (not-applicable)

Durable decisions — never re-adjudicated. Each row has a short durable justification.

| name | reason | recorded |
|------|--------|----------|
| Go builder (`builder: go`) | Rust releaser — cargo is native, not portable from Go toolchain. | 2026-04-16 |
| Go matrix axes (goos/goarch/goarm/goamd64/goarm64/gomips/go386/goppc64/goriscv64/build.ignore/build.buildmode/build.no_main_check) | Go compile-matrix metadata; Rust uses target triples. | 2026-04-16 |
| ldflags / asmflags / gcflags | Go toolchain flags; Rust uses `RUSTFLAGS`+`build.rs`+`[profile.*]`. | 2026-04-16 |
| build.tags | Go build tags; Rust uses `--features`. | 2026-04-16 |
| gomod (proxy/env/mod/gobinary/dir) | Go module proxying; replaced by `Cargo.lock` + `cargo metadata`. | 2026-04-16 |
| Zig / Bun / Deno / Python-uv / Python-poetry builders | Non-Rust language runtimes; no Rust analogue (zig-as-linker is covered via cargo-zigbuild under `tool`). | 2026-04-16 |
| ko (Go-source-to-container) | Go-source container image; `docker` + `docker_v2` cover the Rust case. | 2026-04-16 |
| npm publish | JS/TS runtime registry; no canonical Rust parallel. Project-specific JS wrappers remain opt-in. | 2026-04-16 |
| Pro license flag (`--key`) | Pro licensing mechanism; anodizer is OSS, no analogue. | 2026-04-16 |
| GGOOS / GGOARCH (split filter) | Go matrix axes; Rust-native replacement is target-triple filtering via `ANODIZER_SPLIT_TARGET`. | 2026-04-16 |

---

## 5. Completion / Gaps / Bloat

### Rust-appropriate gaps (parity_status ∈ {partial, missing}, ecosystem_relevance ∈ {required, strongly-suggested})

**Zero blocking rows.** The 11 rows flagged in the 2026-04-16 A5 countersign were remediated by 2026-04-18; all ecosystem_relevance ∈ {required, strongly-suggested} rows now show `parity_status = implemented` with field-level citation. See §5.closures for the evidence trail.

### Other missing rows (non-blocking, niche)

| name | status | ecosystem | gap |
|------|--------|-----------|-----|
| `goreleaser man` (man page generation) | missing | niche | Not a blocker. `clap_mangen` would be the implementation path. |
| `--soft` flag on check | missing | niche | Pro feature; anodizer check is strict. |
| `continue_on_error` per-stage | missing | niche | Anodizer is fail-fast. |
| `metadata.full_description.from_url` | partial | niche | Parse works; `from_url` resolution deferred — `FromFile` + `Inline` cover the common cases. core/src/context.rs:754 returns an explicit error on `FromUrl`. |
| `mcp registry` (MCP server manifest publish) | missing | niche | New 2026-03+ GoReleaser pipe; MCP ecosystem still forming. No Rust consumer demand surfaced yet. |

### Bloat candidates (implemented ∧ not-applicable)

No rows qualify. Every `not-applicable` row is `parity_status=n-a`, meaning anodizer does **not** implement it. There is no bloat to disposition.

### 5.closures — A1-rev remediation evidence (2026-04-16 → 2026-04-18)

Each row below was flagged `partial` or `missing` in the 2026-04-16 A5 pro-skeptic countersign; re-verified 2026-04-18 against current source.

| row | 2026-04-16 status | 2026-04-18 evidence | new status |
|-----|-------------------|---------------------|------------|
| `archives[].hooks.before/after` | partial (serde-alias absent → silent skip) | `BuildHooksConfig.pre`/`post` carry `#[serde(alias="before")]` / `alias="after"` (core/src/config.rs:979,982) | implemented |
| `release.header.from_url` / `from_file` | partial (naked `String`, no headers/template) | `ContentSource::FromUrl { from_url, headers }` (config.rs:1376) + body-render in stage-release/src/lib.rs:631 | implemented |
| `release.footer.from_url` / `from_file` | partial (shared `ContentSource`) | Same wiring — verified shared resolver | implemented |
| `nfpm.if` | missing | `NfpmConfig.if_condition` #[serde(rename="if")] (config.rs:2980); filter at stage-nfpm/src/lib.rs:983 | implemented |
| `nfpm.templated_contents` | missing | `NfpmConfig.templated_contents` (config.rs:2986) | implemented |
| `nfpm.templated_scripts` | missing | `NfpmConfig.templated_scripts` (config.rs:2991) | implemented |
| `dmgs[].if` | partial | `DmgConfig.if_condition` (config.rs:3521); gate stage-dmg/src/lib.rs:156 | implemented |
| `msis[].if`, `msis[].hooks.before/after` | partial | `MsiConfig.if_condition` (3555) + `MsiConfig.hooks: BuildHooksConfig` (3560); gate stage-msi/src/lib.rs:291. `extra_files: Vec<String>` matches docs (Wix context filenames). `goamd64` reclassified n-a (Go-specific; Rust uses target triples). | implemented |
| `pkgs[].if` | partial | `PkgConfig.if_condition` (config.rs:3597); gate stage-pkg/src/lib.rs:116 | implemented |
| `nsis[].if` | partial | `NsisConfig.if_condition` (config.rs:3631); gate stage-nsis/src/lib.rs:124. `goamd64` reclassified n-a. | implemented |
| `app_bundles[].if` | partial | `AppBundleConfig.if_condition` (config.rs:3667); gate stage-appbundle/src/lib.rs:263 | implemented |
| `metadata.full_description` + `commit_author` | partial | `MetadataConfig.full_description: ContentSource` + `.commit_author: CommitAuthorConfig` (config.rs:4497,4501); wired in core/src/context.rs:737-774. (FromUrl path still deferred — niche row retained.) | implemented (FromUrl niche partial) |
| CLI `--prepare` flag | missing | `ReleaseOpts.prepare: bool` (commands/release/mod.rs:48); `apply_prepare_mode_to_skip` adds release/publish/announce to skip | implemented |

Since the A5 countersign marked these as BLOCKERs and they are all now closed at the source level, the inventory's `Completion achieved` flips to `yes` — verification of wiring-plus-tests is a continuing parity-audit responsibility (A2/A3/A5), but the A1 inventory contract is satisfied.

### 5.delta — 2026-05-07 refresh (ROLLBACK on completion)

The 2026-05-07 walk of `v2.15.0..HEAD` identifies 1 new required-tier missing row (`retry:` global config) and ~14 partial rows where anodizer must update behavior to track upstream. Completion **flips back to `no`** for this refresh cycle.

| row | 2026-04-18 status | 2026-05-07 evidence | new status |
|-----|-------------------|---------------------|------------|
| `retry:` (project-level) | n/a (didn't exist upstream) | New v2.15.3 (#6528, commit a176567); cross-pipe retry; deprecates `dockers.retry`/`docker_manifests.retry`/`dockers_v2.retry` | **missing (required)** |
| `archives.formats: xz` | n/a (didn't exist upstream) | New v2.15+ (commit bb532b6); single-file format | **missing (niche)** |
| `signs[].artifacts: all` includes Sig+Cert | implemented | GR fix #6509 excluded them; anodizer still includes (sign-test asserts inclusion) | **partial (required)** |
| `release.publish` un-draft body preserves prerelease/name/make_latest | implemented | GR fixes #6591 + 2e17678 expanded PATCH body; anodizer body is minimal | **partial (required)** |
| GitHub author lookup via Search Users API | implemented (superset, fallback) | GR removed in #6601; anodizer still calls Search Users — divergence | **partial (required)** |
| github prerelease forces `make_latest=false` | implemented (template-only) | GR fix #6591: auto-suppress make_latest for prereleases | **partial (required)** |
| `homebrew_casks[]` per-arch sha256/url ordering | implemented | GR fix 87b542b reordered; anodizer still emits url-then-sha256 | **partial (strongly-suggested)** |
| `homebrew_casks.generate_completions_from_executable` | implemented | Config-only; cask template doesn't render the directive | **partial (niche)** |
| `dockers/v2 parsePlatform` nil-safe | implemented | GR commit 9e9f87c | **partial (strongly-suggested)** — verify in stage-docker/v2 |
| `dockers/v2` Dockerfile-template emptiness check after rendering | implemented | GR commit d788340 | **partial (required)** — verify in stage-docker/v2 |
| `dockers v1` healthcheck delegates to v2 when `use: buildx` | implemented | GR commit e09e23a | **partial (strongly-suggested)** — verify in stage-docker |
| sec sweep: redact target/request URL, no env-vars/git-remote/auth/webhook-headers in logs, redact length≥1 | implemented | GR commit d1cdbb2 | **partial (required)** — sweep anodizer log statements |
| `sbom` artifact name uses matched filename | implemented | GR commit 292203e | **partial (required)** — verify in stage-sbom |
| `aur`/`aursources`/`krew` template `skip_upload` before bool check | implemented | GR commit cba5b9f | **partial (strongly-suggested)** — verify all three publishers |
| `release` log uses correct repo for gitlab/gitea | implemented | GR commit 44133de | **partial (strongly-suggested)** — verify TokenType-driven repo selection |
| `mattermost` defaults `Color` from own struct | implemented | GR commit 7e7f9b2 (#6533) | **partial (niche)** — verify in stage-announce/mattermost |
| `linkedin`/`webhook`/`opencollective` error handling | implemented | GR commits e31f01d/0944b0e/bba909e/206120a | **partial (niche)** — mirror error wrapping |
| `snapcraft` retries upload on 5xx | partial (no retry) | GR commit eb944f9; supersede via Project.Retry once implemented | **partial (niche)** |
| `blob` provider templated before S3-ACL gate | implemented | GR commit 4d1924d | **partial (niche)** — verify in stage-blob |
| `gitea` create-file falls back to server default branch | implemented | GR commit 4a9d25f | **partial (niche)** — verify in stage-publish gitea path |
| GitHub `updateRelease` nil-guards `resp` | implemented | GR commit 1ca21f0 | **partial (niche)** — verify octocrab path |
| Rate-limit checker iterative + ctx-cancellable | implemented | GR commit 60028b1 | **partial (strongly-suggested)** — verify in stage-release/github/rate_limit.rs |
| `git config` extraction preserves underlying error | implemented | GR commit 5042b84 | **partial (niche)** — verify in core/src/git/config.rs |
| `redact.Writer` returns 0 bytes on inner-write failure | implemented | GR commit f48613d | **partial (niche)** — verify in anodizer redact layer |
| `bodyOf` returns descriptive error on ReadAll failure | implemented | GR commit 8b77358 | **partial (niche)** — verify HTTP body-reader paths |
| `mcp registry` global-retry integration | missing | GR mcp.go now uses retryx.Do(ctx.Config.Retry, ...) | **missing (niche)** — depends on Project.Retry implementation |

n-a (rust-native-replacement) rows added 2026-05-07: builder:node (Node SEA), gomod proxy retry, partial pipe ppc64le/GOMIPS64 env mapping, build per-binary IDs for ./..., build allow explicit binary with ellipsis, bun parse-error message.



---

## 6. Reference tables (preserved)

These tables remain for auditor reference — fields, defaults, env vars, CLI flags. Authoritative but informational; parity conclusions are drawn from Section 2.

### 6.1 Builder Types
- **Go** (default) · **Rust** (`builder: rust`) · **Zig** · **Bun** · **Deno** · **Python** (coming soon) · **UV** · **Poetry** · **Pre-built/Import** (`builder: prebuilt`)

### 6.2 Rust Builder Fields (anodizer native)
`id`, `builder: rust`, `binary`, `targets[]`, `dir`, `tool (cargo/cross)`, `command (build/zigbuild)`, `flags[] (template)`, `env[] (template)`, `hooks.pre/post[]`, `skip`. Rust defaults: `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin`, `x86_64-pc-windows-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`. Default flags: `--release`.

### 6.3 Archive formats
`tar.gz`, `tgz`, `tar.xz`, `txz`, `tar.zst`, `tzst` (v2.1+), `tar`, `gz`, `zip`, `binary`, `none`

### 6.4 Checksum algorithms
`sha256` (default), `sha512`, `sha1`, `crc32`, `md5`, `sha224`, `sha384`, `sha3-256`, `sha3-512`, `sha3-224`, `sha3-384`, `blake2s`, `blake2b`, `blake3`

### 6.5 Sign artifact scopes
`none`, `all`, `checksum`, `source`, `package`, `installer`, `diskimage`, `archive`, `sbom`, `binary`

### 6.6 nFPM formats & passphrase env priority
Formats: `apk`, `deb`, `rpm`, `termux.deb`, `archlinux`, `ipk`. Passphrase priority: `NFPM_[ID]_[FORMAT]_PASSPHRASE` > `NFPM_[ID]_PASSPHRASE` > `NFPM_PASSPHRASE`.

### 6.7 Publisher channels
Homebrew (brew / cask), Scoop, Chocolatey, Winget, AUR (binary + source), Krew, Nix, Snapcraft, Flatpak, Makeself, Custom publishers, Artifactory, Uploads, Fury.io (Pro), CloudSmith (Pro), NPM (Pro, n-a), crates.io (Rust-additive).

### 6.8 Announce providers
Discord, Slack, Telegram, Teams, Mattermost, Webhook, SMTP, Reddit, Twitter/X, Mastodon, Bluesky, LinkedIn, OpenCollective, Discourse.

### 6.9 Key environment variables
Auth: `GITHUB_TOKEN`, `GITLAB_TOKEN`, `GITEA_TOKEN`, `GORELEASER_FORCE_TOKEN`.
Announcers: `DISCORD_WEBHOOK_ID/TOKEN`, `SLACK_WEBHOOK`, `TELEGRAM_TOKEN`, `TEAMS_WEBHOOK`, `MATTERMOST_WEBHOOK`, `SMTP_*`, `REDDIT_*`, `TWITTER_*`, `MASTODON_*`, `BLUESKY_APP_PASSWORD`, `LINKEDIN_ACCESS_TOKEN`, `OPENCOLLECTIVE_TOKEN`, `DISCOURSE_API_KEY`.
Publishers: `FURY_TOKEN`, `CLOUDSMITH_TOKEN`, `DOCKER_PASSWORD`, `KO_DOCKER_REPO`.
nFPM: `NFPM_*_PASSPHRASE`.

### 6.10 CLI commands
`release`, `build`, `check`, `healthcheck`, `init`, `completion`, `jsonschema`, `changelog` (Pro), `continue` (Pro), `publish` (Pro), `announce` (Pro), `man` (missing in anodizer, niche).

### 6.11 Pro-only features (full list, docs-backed)

1. macOS .pkg, 2. Windows NSIS .exe, 3. Smart SemVer tag sort, 4. NPM registry publishing (n-a), 5. Native macOS codesign/notarize, 6. AI changelog (anthropic/openai/ollama), 7. `if` conditional filters, 8. macOS .app bundles, 9. CloudSmith, 10. Global metadata defaults, 11. Pre-publishing hooks, 12. Cross-platform publishing, 13. DockerHub description sync, 14. macOS .dmg, 15. Windows .msi with Wix, 16. Single-target release building, 17. Template files, 18. `.Artifacts` variable, 19. Split/merge builds, 20. Enhanced changelog (path filtering, subgroups, dividers), 21. Archive hooks, 22. Multi-stage release (prepare/publish/announce), 23. Changelog preview, 24. Nightly builds, 25. Prebuilt binary import, 26. Podman support, 27. GemFury, 28. Includes (config reuse), 29. Global after hooks, 30. Monorepo, 31. Custom template variables, 32. Flatpak, 33. Templated extra_files/contents/scripts/dockerfiles.

### 6.12 Decision rule for ecosystem_relevance

Applied at row-write time (2026-04-16):
1. *Would a reasonable Rust CLI/library author expect anodizer to support this?* If no → `not-applicable`.
2. *Among Rust tools with first-class release tooling (ripgrep, bat, fd, starship, uv, ruff, biome, sea-orm, tauri, cargo-dist), how many support this channel?*
   - 0–1 → `niche`; 2–5 → `strongly-suggested`; 6+ or universal → `required`.

Head-count sources: community READMEs (fetched 2026-04-16 via prior sessions; not re-fetched row-by-row), cargo-dist feature matrix.

---

## Completion statement

- Total GoReleaser OSS features catalogued: 326 (adds 47 new rows from 2026-05-07 §2.18 refresh delta)
- Total GoReleaser Pro features catalogued: 51 (no Pro-tier additions in this refresh)
- Rows with `ecosystem_relevance = required`: 96 (+7: `retry:` global + 6 sec/release/sign/sbom downgrades)
- Rows with `ecosystem_relevance = strongly-suggested`: 134 (+6: docker behavioral fixes + cask ordering + aur/krew template)
- Rows with `ecosystem_relevance = niche`: 119 (+20: cosmetic and observability fixes)
- Rows with `ecosystem_relevance = not-applicable`: 28 (+8: builder:node, partial-pipe Go-matrix env, build ./... ellipsis, bun parse-error, gomod proxy retry, npm Pro)
- anodizer implemented (required): 89/96 (1 missing — Project.Retry; 6 partial — sign-all-exclusion, github-publish PATCH body, github author lookup, prerelease make_latest=false, dockers v2 dockerfile-empty-after-render, sbom matched-filename, sec-sweep)
- anodizer implemented (strongly-suggested): 124/134 (10 partial — see §5.delta)
- niche missings/partials: ~25 — see §2.18.2 + §5.delta
- Completion achieved: **no**
- Reasoning: 2026-05-07 walk against HEAD `8976559` (50 commits since `f7e73e3`) flipped 16 implemented rows to partial and added 1 required missing row (`Project.Retry` global config). The 2026-04-18 completion-yes claim held against `f7e73e3` and is no longer valid. Sessions P + Q appended to parity-session-index carry the actionable list. Not all partials require code changes — several are "verify-in-anodizer" gates (e.g. `parsePlatform` nil-safe; anodizer's Rust pattern-match likely prevents that panic class). Triage required.

## Completion statement (generated)

Parity target: GoReleaser HEAD (commit `8976559`, tag `v2.16.0-17315a55-nightly-1-g8976559`, refreshed 2026-05-07).

- Rust-appropriate features (ecosystem_relevance ∈ {required, strongly-suggested}): 224
  - parity_status=implemented: 207
  - parity_status=partial:     16   ← blocks completion (see §5.delta)
  - parity_status=missing:     1    ← blocks completion (`retry:` global)
- Bloat (implemented ∧ not-applicable): 0
  - dispositioned:  0
  - undecided:      0
  - resolved (disposition executed): 0
  - unresolved:     0
- Rust-additive rows: 12 (§3; enumerated for dogfooding matrix)
- Permanent negative space (ecosystem_relevance=not-applicable): 28 (added 7 in 2026-05-07 refresh: builder:node + 6 Go-matrix/n-a items)

Completion achieved: **no — 17 rows downgraded by 2026-05-07 walk; tracked in `parity-session-index.md` Session P+Q**.

The 2026-04-18 completion-yes claim held against `f7e73e3`; the 2026-05-07 walk against HEAD `8976559` flipped 16 implemented rows to partial (mostly behavioral fixes upstream applied 2026-03 → 2026-05) and added 1 new required missing row (`Project.Retry` global config — commit a176567, v2.15.3). Rust-additive features remain ahead of upstream where they add real UX value.

**Auditor note.** Sessions P (Refresh-driven gaps) and Q (Action-layer immutable-nightly) appended to `parity-session-index.md` carry the actionable list. Not all 16 partials require code changes — several are "verify-in-anodizer" gates (e.g. nil-safe Docker parsePlatform; anodizer's Rust pattern-match likely already prevents that panic class). A2/A3 spec-comparison agents should walk those gates first to convert verify-only rows back to `implemented`.
