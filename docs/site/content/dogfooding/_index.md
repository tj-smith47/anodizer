+++
title = "Dogfooding"
description = "Every anodize feature, each with status and proof — what works, what doesn't, where to find the evidence."
weight = 30
template = "section.html"
+++

# Feature Dogfooding Matrix

This page lists every feature tracked in the [GoReleaser parity inventory](https://github.com/tj-smith47/anodize/blob/master/.claude/specs/goreleaser-complete-feature-inventory.md) plus the Rust-additive surface (§3), each with a verifiable status. "Tested" means there is a CI run, unit/integration test, or live release that exercised the feature end-to-end. "Partial" means the code exists with unit tests but has not been proven by a live release or has a known gap. "Untested" means the feature is not implemented or has no evidence on disk.

Two public projects provide the live dogfood proof:

- [anodize releases](https://github.com/tj-smith47/anodize/releases) — seven releases (v0.1.1 through v0.2.5, all 2026-04-12 to 2026-04-15), cross-compiled via `anodize release --split` on three OS matrices.
- [cfgd releases](https://github.com/tj-smith47/cfgd/releases) — four simultaneous workspace releases on 2026-04-15 (`v0.3.5`, `core-v0.3.5`, `operator-v0.3.5`, `csi-v0.3.5`) through [anodize-action](https://github.com/tj-smith47/anodize-action).

Evidence files (one per feature group) live in [`.claude/audits/2026-04-v0.x/`](https://github.com/tj-smith47/anodize/tree/master/.claude/audits/2026-04-v0.x). Open blockers and partial-parity entries live in [`.claude/known-bugs.md`](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md).

**Legend.** ✅ tested end-to-end (unit + integration + CI, often + live release) · ⚠ partial (implemented with unit tests, no live-release proof, or known field gap) · ❌ untested (not implemented or no evidence).

## Summary

| Bucket | ✅ tested | ⚠ partial | informational |
|---|---|---|---|
| Evidence file clusters (19 files) | 11 | 7 | 1 |
| Parity inventory rows covered | 217 OSS+Pro rows + 27 anodize-action rows | — | — |

Per-feature-group counts match the [evidence index](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-INDEX.md).

Anchors flagged by the audit as needing a live release (not blocked by missing code) are called out in the [Outstanding live-release gaps](#outstanding-live-release-gaps) section at the bottom.

## Canonical proof URLs

Reused across rows below:

- **anodize CI** (latest success, commit `128e003`, 2026-04-15) — [run 24441674093](https://github.com/tj-smith47/anodize/actions/runs/24441674093)
- **anodize release v0.2.5** — [run 24441952862](https://github.com/tj-smith47/anodize/actions/runs/24441952862) · [release tag](https://github.com/tj-smith47/anodize/releases/tag/v0.2.5)
- **cfgd core-v0.3.5** (lib) — [run 24442229349](https://github.com/tj-smith47/cfgd/actions/runs/24442229349)
- **cfgd v0.3.5** (CLI) — [run 24442230191](https://github.com/tj-smith47/cfgd/actions/runs/24442230191)
- **cfgd operator-v0.3.5** — [run 24442230834](https://github.com/tj-smith47/cfgd/actions/runs/24442230834)
- **cfgd csi-v0.3.5** — [run 24442232044](https://github.com/tj-smith47/cfgd/actions/runs/24442232044)
- **anodize-action CI** — [run 24409150253](https://github.com/tj-smith47/anodize-action/actions/runs/24409150253)

## Build

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-builds.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| builder: rust (cargo / cross / cargo-zigbuild) | ✅ | OSS | [anodize release run](https://github.com/tj-smith47/anodize/actions/runs/24441952862) split jobs ran `anodize release --split --clean` on ubuntu/macos/windows runners. |
| builder: prebuilt | ✅ | OSS | `stage-build` unit tests; [CI run 24441674093](https://github.com/tj-smith47/anodize/actions/runs/24441674093). |
| build.id / binary / dir / command / flags / env / tool | ✅ | OSS | Covered by `test_e2e_build_command_matches_goreleaser_pipeline_outputs` in [`integration.rs`](https://github.com/tj-smith47/anodize/blob/master/crates/cli/tests/integration.rs) L3617. |
| build.targets (Rust target triples) | ✅ | OSS | Six target triples × seven releases shipped on [v0.2.5](https://github.com/tj-smith47/anodize/releases/tag/v0.2.5). |
| build.overrides (per-target) | ✅ | OSS | `BuildOverride` array wired per-target; unit tests in `stage-build`. |
| build.hooks.pre / post | ✅ | OSS | `test_e2e_before_hooks_execute` at `integration.rs` L3368. |
| build.mod_timestamp (reproducible build) | ✅ | OSS | `stage-build` unit tests cover `CrateConfig.mod_timestamp` wiring. |
| build.skip (templated bool) | ✅ | OSS | `BuildConfig.skip` unit tests. |
| build.no_unique_dist_dir | ✅ | OSS | `stage-build` unit tests. |
| universal_binaries (macOS `lipo`) | ✅ | OSS | `UniversalBinaryConfig` subprocess tests in `stage-build`. |
| upx (per-target filtering via Rust target globs) | ✅ | OSS | `stage-upx` unit tests; v0.2.5 binaries are UPX-compressed (config at [`.anodize.yaml:488`](https://github.com/tj-smith47/anodize/blob/master/.anodize.yaml#L488)). |
| prebuild pipe (pre-build validation) | ✅ | OSS | Folded into `anodize build` stage; CI run 24441674093. |
| reportsizes | ✅ | OSS | `test_e2e_report_sizes` at `integration.rs` L3546. |
| `--single-target` (partial build) | ✅ | OSS | CI snapshot job runs `anodize release --snapshot --single-target --clean --dry-run` on every master push. |

## Archive

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-archives.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| archives[].id / ids | ✅ | OSS | `test_parse_archive_*` ~40 config tests in `config_parsing_tests.rs`. |
| archives[].format (singular, deprecated) + formats (plural v2.6+) | ✅ | OSS | Both shapes accepted; `stage-archive` unit tests. |
| archives[].name_template | ✅ | OSS | v0.2.5 ships [`anodize-0.2.5-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodize/releases/tag/v0.2.5) rendered from `{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}.tar.gz`. |
| archives[].wrap_in_directory (bool/string + template) | ✅ | OSS | `test_wrap_in_directory_*` suite in `stage-archive/src/lib.rs`. |
| archives[].strip_binary_directory | ✅ | OSS | `stage-archive` unit tests. |
| archives[].allow_different_binary_count | ✅ | OSS | `stage-archive` unit tests. |
| archives[].files (string + object shape) | ✅ | OSS | `ArchiveFileSpec` enum parses both forms; unit tests. |
| archives[].builds_info (file mode/owner/group/mtime) | ✅ | OSS | `stage-archive` unit tests. |
| archives[].format_overrides (Windows = zip) | ✅ | OSS | `test_format_for_target_multiple_overrides` L2561; v0.2.5 ships `.zip` on Windows. |
| archives[].templated_files | ✅ | Pro | `TemplatedExtraFile` wired; cfgd ships a rendered `install.sh` via `template_files`. |
| archives[].meta (manifest-only) | ✅ | OSS | `ArchiveConfig.meta` unit tests. Note: `meta: true` with zero matches emits empty archive (BLOCKER — [known-bugs #58](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md)). |
| archives[].hooks.before / after | ⚠ | Pro | Field-name mismatch — anodize uses `hooks.pre`/`post`; docs use `before`/`after`. Silent skip. [known-bugs A1-rev #32](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| formats: tar.gz / tgz / tar.xz / txz / tar.zst / tzst / tar / gz / zip / binary / none | ✅ | OSS | Per-format integration tests (`test_integration_tar_gz_realistic_file_tree` L2284, `_zip` L2339, `_tar_xz` L2382, `_tar_zst` L2468). v0.2.5 ships tar.gz + zip archives live. |
| source archive | ✅ | OSS | v0.2.5 ships `anodize-0.2.5-source.tar.gz`. |
| source.templated_files | ✅ | Pro | `stage-source` unit tests; used by cfgd. |

## Checksum

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-checksums.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| checksum.name_template | ✅ | OSS | v0.2.5 ships `anodize-0.2.5-checksums.txt`. |
| checksum.algorithm (sha256 / sha512 / sha1 / sha224 / sha384 / sha3-* / blake2s / blake2b / blake3 / crc32 / md5) | ✅ | OSS | Unit tests in `stage-checksum/src/lib.rs`; [CI run 24441674093](https://github.com/tj-smith47/anodize/actions/runs/24441674093). |
| checksum.split (per-artifact sidecar) | ✅ | OSS | `stage-checksum` unit tests. |
| checksum.disable | ✅ | OSS | `test_check_global_checksum_disable_valid` L1702 + `test_check_per_crate_checksum_disable_valid` L1641. |
| checksum.ids | ✅ | OSS | `test_parse_checksum_ids` L1254. |
| checksum.extra_files | ✅ | OSS | `test_parse_checksum_extra_files` L1235. |
| checksum.templated_extra_files | ✅ | Pro | `stage-checksum` unit tests. |

## Release (SCM providers)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-release-scm.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| release.github (full semantics) | ✅ | OSS | Seven anodize releases + four cfgd releases. [v0.2.5](https://github.com/tj-smith47/anodize/releases/tag/v0.2.5). |
| release.gitlab | ⚠ | OSS | `stage-release/src/gitlab.rs` multi-client unit tests; no live GitLab project in dogfood. |
| release.gitea | ⚠ | OSS | `stage-release/src/gitea.rs` unit tests; no live Gitea project in dogfood. |
| release.draft / replace_existing_draft / use_existing_draft / replace_existing_artifacts | ✅ | OSS | `stage-release` unit tests; cfgd `.anodize.yaml:158` sets `draft: false`. |
| release.target_commitish / discussion_category_name | ✅ | OSS | `stage-release` unit tests. |
| release.tag (template) | ✅ | Pro | Seven anodize releases used `tag_template` (Tera-backed). |
| release.prerelease (auto/bool) | ✅ | OSS | cfgd config sets `prerelease: auto`, confirmed on [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5). |
| release.make_latest | ✅ | OSS | cfgd config sets `make_latest: auto`. |
| release.mode (keep-existing / append / prepend / replace) | ✅ | OSS | `stage-release` unit tests. |
| release.header / footer (string) | ✅ | OSS | Release body on v0.2.5 renders "Released with anodize" footer template. |
| release.header.from_url / from_file + release.footer.from_url / from_file | ⚠ | Pro | `ContentSource::FromUrl` is a naked `String` (no headers/auth/template-render). [known-bugs A1-rev #34](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| release.name_template / disable / skip_upload | ✅ | OSS | `stage-release` unit tests. |
| release.extra_files / templated_extra_files | ✅ | OSS+Pro | `stage-release` unit tests. |
| release.include_meta (metadata.json / artifacts.json emission) | ✅ | OSS | v0.2.5 ships `metadata.json` (asset id `RA_kwDORxhcIs4Xp2mB`) + `artifacts.json` live. |
| Enterprise URL overrides (github_urls / gitlab_urls / gitea_urls) | ✅ | OSS | `stage-release` unit tests. |
| milestone pipe | ✅ | OSS | `crates/cli/src/commands/release/milestones.rs` (split from `release/mod.rs` 2026-04-16). |

## Changelog

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-changelog.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| changelog.disable | ✅ | OSS | `test_check_changelog_disabled_valid` L1734. |
| changelog.use (git / github / gitlab / gitea / github-native) | ✅ | OSS | `stage-changelog` unit tests; v0.2.5 + cfgd v0.3.5 rendered changelogs live. |
| changelog.format template | ✅ | OSS | `test_e2e_changelog_header_footer` L3091. |
| changelog.sort (asc/desc), abbrev | ✅ | OSS | cfgd uses `sort: asc` live. |
| changelog.paths (monorepo filter) | ✅ | Pro | cfgd 4-workspace monorepo relies on per-crate changelog filter. |
| changelog.title (v2.12+), divider | ✅ | Pro | `stage-changelog` unit tests. |
| changelog.filters.include / exclude | ✅ | OSS | v0.2.5 body shows `^docs:/^ci:/^chore:/^style:` filters applied. |
| changelog.groups[].title / regexp / order + groups[].groups[] (subgroups) | ✅ | OSS+Pro | `test_e2e_changelog_with_groups` L1964; v0.2.5 body shows Features / Bug Fixes / Others groups live. |
| changelog.ai.use / model / prompt (anthropic / openai / ollama) | ⚠ | Pro | Implemented in `stage-changelog`; **no live release uses `use: ai`**. Flagged in [HANDOFF](#outstanding-live-release-gaps). |

## Signing

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-signing.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| signs[] (gpg backend) | ✅ | OSS | v0.2.5 ships `anodize-0.2.5-checksums.txt.sig` live. |
| signs[] (cosign backend) | ✅ | OSS | v0.2.5 ships `anodize_linux_amd64.sig`, `_arm64.sig`, `_darwin_amd64.sig`, `_darwin_arm64.sig`, `.exe_windows_amd64.sig`, `_arm64.sig` — six cosign binary sigs. |
| signs[].cmd / signature / args (templated) | ✅ | OSS | `stage-sign` unit tests. |
| signs[].artifacts (none / all / checksum / source / package / installer / diskimage / archive / sbom / binary) | ✅ | OSS | Scope filter covered by `stage-sign` unit tests. |
| signs[].ids | ✅ | OSS | `stage-sign` unit tests. |
| signs[].if | ⚠ | Pro | Implemented; no live release uses conditional signs. |
| signs[].stdin / stdin_file / certificate (cosign/rekor) / env / output | ✅ | OSS+Pro | `stage-sign` unit tests. |
| docker_signs[] (cosign on docker manifests) | ✅ | OSS | cfgd signs three ghcr.io images on [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191). |
| binary_signs[] (`BinarySignStage` build-time signing) | ✅ | OSS | Added 2026-04-16; inline tests in `crates/stage-sign/src/lib.rs`. v0.2.5 cosign binary sigs are the live proof. |

## Docker

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-docker.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| dockers[] (v1 legacy) | ✅ | OSS | `stage-docker` unit tests. |
| docker_v2[] (modern) | ✅ | OSS | cfgd ships three images via `docker_v2`: [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191) (agent), [operator-v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230834), [csi-v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442232044). |
| docker.image_templates + dockerfile | ✅ | OSS | cfgd three-image config at [`.anodize.yaml:232`](https://github.com/tj-smith47/cfgd/blob/master/.anodize.yaml#L232). |
| templated_dockerfile + templated_extra_files | ✅ | Pro | `stage-docker` unit tests. |
| extra_files + build_flag_templates | ✅ | OSS | `stage-docker` unit tests; cfgd uses `build_args` live. |
| use (docker / buildx / podman) | ⚠ | OSS | buildx + docker daemon exercised in CI; podman path unit-tested only. |
| skip_build (Pro), skip_push, push_flags, retry | ✅ | OSS+Pro | `stage-docker` retry/backoff unit tests. |
| docker_v2.platforms (linux/amd64 + linux/arm64 multi-arch) | ✅ | OSS | cfgd ships multi-arch manifests live. |
| docker_v2.sbom (inline) | ✅ | OSS | cfgd sets `sbom: true` on three images live. |
| docker_v2.labels / annotations / build_args | ✅ | OSS | cfgd uses all three live. |
| docker_manifests[] | ✅ | OSS | cfgd config declares three manifests live. |
| dockerdigest | ✅ | OSS | cfgd sets `docker_digest.name_template` at `.anodize.yaml:261`. |
| dockerhub (description sync) | ⚠ | Pro | 14 unit tests in `stage-publish/src/dockerhub.rs`; cfgd/anodize use ghcr.io, not Docker Hub — **no live dogfood**. |

## Linux packaging (nFPM + srpm)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-nfpm.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| nfpms[] (id / ids / package_name / file_name_template) | ✅ | OSS | v0.2.5 ships `anodize_0.2.5_linux_amd64.deb` + arm64 + rpm + apk live. |
| formats: deb / rpm / apk | ✅ | OSS | v0.2.5 ships all three for amd64 + arm64 (six nfpm artifacts). |
| formats: archlinux / ipk / termux.deb | ⚠ | OSS | `stage-nfpm` format dispatch unit-tested; **not shipped live** by anodize/cfgd. |
| vendor / homepage / maintainer / description / license | ✅ | OSS | cfgd + anodize configs populate all five; visible in live nfpm assets. |
| umask / bindir / libdirs | ✅ | OSS | cfgd sets `bindir: /usr/bin` live. |
| epoch / prerelease / version_metadata / release / section / priority | ✅ | OSS | `stage-nfpm` unit tests. |
| meta / changelog / goamd64 / mtime | ✅ | OSS | `stage-nfpm` unit tests. |
| dependencies / provides / recommends / suggests / conflicts / replaces | ✅ | OSS | `stage-nfpm` unit tests. |
| contents[] with file_info | ✅ | OSS | cfgd maps `LICENSE`, `README` into `/usr/share/doc/cfgd/` live. |
| scripts (preinstall / postinstall / preremove / postremove) | ✅ | OSS | `stage-nfpm` unit tests. |
| rpm.* / deb.* / apk.* / archlinux.* / ipk.* blocks | ✅ | OSS | Per-packager unit tests. |
| overrides | ✅ | OSS | `stage-nfpm` unit tests. |
| ConventionalFileName (per-packager v2.44 closure) | ✅ | OSS | `stage-nfpm/src/filename.rs` (added 2026-04-16); live filenames on v0.2.5 match per-format conventions. |
| passphrase env priority (`NFPM_[ID]_[FORMAT]_PASSPHRASE` > `NFPM_[ID]_PASSPHRASE` > `NFPM_PASSPHRASE`) | ✅ | OSS | `stage-nfpm` unit tests. |
| srpm (rpmbuild subprocess) | ✅ | OSS | v0.2.5 ships `anodize-0.2.5-1.src.rpm` live. |
| nfpm.if (Pro templated conditional) | ❌ | Pro | **Missing** from `NfpmConfig`. [known-bugs A1-rev #28](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| nfpm.templated_contents / templated_scripts | ❌ | Pro | **Missing** — silent serde drop. [known-bugs A1-rev #30](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |

## SBOM + source archive

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-sbom-snap-flatpak-installers.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| sboms[] (syft subprocess) | ✅ | OSS | v0.2.5 ships `anodize-0.2.5.cdx.json` (CycloneDX, 106484 bytes). |
| sboms[].cmd / args / env / artifacts / ids / disable / documents | ✅ | OSS | `stage-sbom` unit tests. |
| `${artifact}` / `${document}` / `${artifactID}` template substitution | ✅ | OSS | `stage-sbom` unit tests. |
| source archive (format / name_template / files / enabled) | ✅ | OSS | v0.2.5 ships `anodize-0.2.5-source.tar.gz`. |
| source.templated_files | ✅ | Pro | Used by cfgd; unit-tested in `stage-source`. |

## Snapcraft / Flatpak / Makeself

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-sbom-snap-flatpak-installers.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| snapcrafts[] (grade / confinement / base / plugs / slots / layout / apps / assumes / hooks) | ✅ | OSS | v0.2.5 ships `anodize_0.2.5_amd64.snap` + `_arm64.snap`; cfgd also publishes to Snap store with `SNAPCRAFT_STORE_CREDENTIALS` secret. |
| flatpaks[] (app_id / runtime / sdk / command / finish_args) | ⚠ | OSS | `stage-flatpak` unit tests; **no live flatpak release** from anodize or cfgd. |
| makeselfs[] (Linux + macOS self-extracting `.run` installers) | ✅ | OSS | v0.2.5 ships four `*-installer.run` files (linux-amd64, linux-arm64, darwin-amd64, darwin-arm64). |

## Pro installers (DMG / MSI / PKG / NSIS / app bundles)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-sbom-snap-flatpak-installers.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| dmgs[] (macOS disk image via hdiutil) | ⚠ | Pro | `stage-dmg` unit tests; **no live `.dmg`** on any release. |
| msis[] (Windows via Wix/wixl) | ⚠ | Pro | `stage-msi` unit tests; **no live `.msi`** on any release. |
| pkgs[] (macOS .pkg) | ⚠ | Pro | `stage-pkg` unit tests; **no live `.pkg`** on any release. |
| nsis[] (Windows NSIS installer) | ⚠ | Pro | `stage-nsis` unit tests; **no live NSIS `.exe`** on any release. |
| app_bundles[] (macOS .app) | ⚠ | Pro | `stage-appbundle` unit tests; **no live `.app`** on any release. |
| dmgs[].if / msis[].if / pkgs[].if / nsis[].if / app_bundles[].if | ❌ | Pro | Missing on all five. [known-bugs A1-rev #41–49](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| msis[].goamd64 / msis[].hooks.before/after / nsis[].goamd64 | ❌ | Pro | Absent. |

## Notarize

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-sbom-snap-flatpak-installers.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| notarize.macos (cross-platform anchore/quill backend) | ⚠ | OSS | `stage-notarize` unit tests; no live release carries a notary ticket. |
| notarize.macos_native (Pro, codesign / xcrun notarytool + keychain) | ⚠ | Pro | `MacOSNativeSignNotarizeConfig` at `core/src/config.rs:3742`; requires Apple Developer cert on macOS runner. Flagged in [HANDOFF D2](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/HANDOFF.md). |

## Homebrew

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-homebrew-cask.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| brews[] / homebrew_formulas[] full surface (~87 unit tests) | ✅ | OSS | cfgd pushes to `tj-smith47/homebrew-tap` live on [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191) with `HOMEBREW_TAP_TOKEN`. |
| repository.* (+ PR-based via pull_request.*, check_boxes Pro) | ✅ | OSS+Pro | Unit tests. |
| commit_author.* + commit signing (v2.11+) | ✅ | OSS | Unit tests. |
| alternative_names | ✅ | Pro | Unit tests. |
| url_template / url_headers / download_strategy / custom_require / custom_block | ✅ | OSS | Unit tests. |
| homepage / description / license / caveats / install / extra_install / post_install / test | ✅ | OSS | cfgd formula ships live with `install` + `test` blocks. |
| dependencies / conflicts | ✅ | OSS | Unit tests. |
| service / plist | ✅ | OSS | Unit tests. |
| brews[].app (DMG integration) | ⚠ | Pro | Implemented; no `.dmg` artifact flowing into a formula yet. |
| homebrew_casks[] full surface (~24 unit tests) | ⚠ | OSS | Unit tests only; cfgd installs via formula, not cask. |

## Windows publishers (Scoop / Chocolatey / Winget)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-scoop-chocolatey-winget.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| scoops[] (manifest + persist/pre_install/post_install/depends/shortcuts + repository.*) | ✅ | OSS | cfgd pushes to `tj-smith47/scoop-bucket` on [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191) with `SCOOP_BUCKET_TOKEN`. |
| chocolateys[] (~21 unit tests, native nupkg path — no choco CLI) | ✅ | OSS | cfgd publishes live with `CHOCOLATEY_API_KEY`. Native nupkg via recent commit `248c904`. |
| wingets[] (~24 unit tests, manifests_repo PR flow) | ✅ | OSS | cfgd pushes to `tj-smith47/winget-pkgs` (`TJSmith.cfgd`) live with `WINGET_PKGS_TOKEN`. |

## AUR / Krew / Nix

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-aur-krew-nix.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| aurs[] (12 unit tests, Arch PKGBUILD rendering) | ⚠ | OSS | Unit tests only; **no live AUR package** from anodize/cfgd. |
| aur_sources[] (source-based AUR, 5 unit tests) | ⚠ | OSS | Unit tests only; not dogfooded. |
| krews[] (10 unit tests) | ✅ | OSS | cfgd pushes to `krew-index` live with `KREW_INDEX_TOKEN`. Recent fix `128e003` "krew default upstream + per-crate previous_tag prefix filter" confirmed at CI 24441674093. |
| nix[] (14 unit tests, ELF architecture parsing) | ✅ | OSS | cfgd pushes to `nix-pkgs` live with `NIX_PKGS_TOKEN`. |

## Publishers (custom / cloud / blob / registry)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-publish-misc-blob.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| publishers[] (custom cmd, 30+ unit tests) | ✅ | OSS+Pro | `test_e2e_custom_publishers_dry_run` L2387. |
| crates.io publish (Rust-additive) | ✅ | Rust-additive | cfgd publishes four crates live 2026-04-15: [core-v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442229349), [v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191), [operator-v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230834), [csi-v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442232044) with `CARGO_REGISTRY_TOKEN`. |
| binstall metadata (Rust-additive) | ✅ | Rust-additive | cfgd `binstall.enabled: true` + `pkg_url`/`pkg_fmt` shipped in v0.3.5. |
| blobs (s3 / gs / azblob, object_store SDK) | ⚠ | OSS+Pro | `stage-blob` + ~30 util unit tests; **no live release** pushes to cloud storage — no AWS/GCP/Azure creds in any workflow. |
| artifactory (target / mode / TLS / headers / matrix Pro) | ⚠ | OSS+Pro | 30 unit tests in `stage-publish/src/artifactory.rs`; **not dogfooded live**. |
| uploads[] (generic HTTP) | ⚠ | OSS | Unit-tested; **not dogfooded live**. |
| fury (Pro) | ⚠ | Pro | Unit-tested; **not dogfooded live**. |
| cloudsmith (Pro, 24 unit tests) | ⚠ | Pro | Unit-tested; **not dogfooded live**. |

## Announcers (13 channels)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-announce.md)

Of 13 channels, **2 are dogfooded live** (webhook + smtp via cfgd), the remaining 11 have passing unit tests only — no workflow has posted a release announcement to them.

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| webhook (custom HTTP broadcast, 6 unit tests) | ✅ | OSS | cfgd posts to `https://tj.jarvispro.io/webhooks/anodize` live on [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191). |
| email / smtp (6 unit tests) | ✅ | OSS | cfgd sends via `smtp.gmail.com:587` live with `SMTP_PASSWORD` secret. Note: `smtp_port.unwrap_or(587)` silent default — BLOCKER [known-bugs #109](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| discord (4 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| slack — channel / blocks / attachments (3 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| telegram — parse_mode MarkdownV2/HTML + message_thread_id (5 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| teams (AdaptiveCard, 6 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| mattermost (7 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| reddit | ⚠ | OSS | Unit-tested; no live announce. |
| twitter / X (8 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| mastodon (form-encoded POST) | ⚠ | OSS | Unit-tested; no live announce. |
| bluesky (AT Proto, 2 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |
| linkedin (1 unit test) | ⚠ | OSS | Unit-tested; no live announce. |
| opencollective (1 unit test) | ⚠ | OSS | Unit-tested; no live announce. |
| discourse (2 unit tests) | ⚠ | OSS | Unit-tested; no live announce. |

## Project / metadata / env / snapshot / nightly

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-project-global-cli.md) · [env tokens](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-artifacts-metadata-auth.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| project_name / dist | ✅ | OSS | `test_parse_project_name_*` L17–L52, `test_parse_dist_*` L62–L99. |
| env (global list) + env_files (github_token / gitlab_token / gitea_token) | ✅ | OSS | cfgd sets `env: [REGISTRY=ghcr.io, RELEASE_TYPE=stable]` + tokens live. |
| variables (custom `.Var.*`) | ✅ | OSS | cfgd uses `.Var.repo_url` / `.Var.description` across config live. |
| template_files[] | ✅ | Pro | cfgd renders `install.sh` from template live (ships in release). |
| includes[].from_file | ✅ | Pro | Unit-tested; cfgd/anodize use single-file config. |
| includes[].from_url | ⚠ | Pro | Struct implemented + unit-tested; **no live config pulls a remote include**. |
| snapshot.name_template + `--auto-snapshot` | ✅ | OSS | `test_e2e_auto_snapshot_dirty_repo` L3322, `test_e2e_snapshot_version_in_artifacts` L2587. |
| metadata.{homepage / license / description / maintainers / mod_timestamp} | ⚠ | Pro | Collected but not consumed — config-without-wiring. [known-bugs A1-rev #39](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| metadata.full_description.from_url / from_file + metadata.commit_author | ❌ | Pro | **Missing**. [known-bugs A1-rev #37](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| nightly (NightlyConfig + `--nightly` flag) | ⚠ | Pro | Wired; **no live nightly release exists** — release tag list has only semver `v*` tags. |

## Hooks (before / after / build / tag)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-project-global-cli.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| before + after global hooks | ✅ | OSS | `test_e2e_before_hooks_execute` L3368, `test_e2e_before_hooks_dry_run` L3437; cfgd uses both live. |
| build.hooks.pre / post | ✅ | OSS | `core/src/hooks.rs` unit tests; commit `248c904` "skip before hooks on tag-triggered CI" documents gated behavior. |
| tag_pre_hooks / tag_post_hooks (Rust-additive, templated vars) | ✅ | Rust-additive | `cli/src/commands/tag.rs` inline tests; anodize CI auto-tag step runs `anodize tag` live. |
| archives[].hooks.before / after | ⚠ | Pro | Field-name mismatch (`pre`/`post` vs `before`/`after`); silent skip. [known-bugs A1-rev #32](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |

## Partial builds (split / merge)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-project-global-cli.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| partial.by (goos / goarch / target) | ✅ | OSS | cfgd `.anodize.yaml:527` sets `partial.by: goos` in production. |
| `--split` flag | ✅ | OSS | anodize v0.2.5 [run 24441952862](https://github.com/tj-smith47/anodize/actions/runs/24441952862) — three split build jobs (ubuntu/macos/windows) succeeded. |
| `--merge` flag | ✅ | OSS | cfgd release.yml publish job runs `release --merge --crate <workspace>` on every tag; [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/actions/runs/24442230191) succeeded. |
| `--id` (Pro crate filter) | ✅ | Pro | cfgd uses `--crate <workspace>` live across four workspace releases. |
| `--prepare` (Pro multi-stage) | ❌ | Pro | **Missing**. [known-bugs A1-rev #51](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). Blocks Pro `prepare → publish → announce` flow. |
| `ANODIZE_SPLIT_TARGET` env (Rust-native replacement for GGOOS/GGOARCH) | ✅ | Rust-additive | Consumed by split jobs; shipped in v0.2.5. |
| context.json serialization across workers | ✅ | OSS | Live in run 24441952862 (three split context files produced + merged). Edge case: mid-write truncation error messaging flagged in [HANDOFF D1](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/HANDOFF.md). |

## Git + monorepo + workspaces

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-project-global-cli.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| git.tag_sort (-version:refname / semver / smartsemver Pro) | ✅ | OSS+Pro | anodize `.anodize.yaml:24` sets `git.tag_sort`; unit tests in `core/src/git.rs`. |
| git.prerelease_suffix | ✅ | OSS | `core/src/git.rs` unit tests. |
| git.ignore_tags / ignore_tag_prefixes | ✅ | OSS+Pro | Unit tests. |
| monorepo.tag_prefix / dir | ✅ | OSS | cfgd 4-workspace monorepo (`core-v*`, `v*`, `operator-v*`, `csi-v*`) released in parallel 2026-04-15 — all four tag patterns resolved correctly. |
| workspaces[] (Rust-additive, tag → crate resolution) | ✅ | Rust-additive | `test_e2e_workspace_*` suite (L978, L1046, L1119, L1184, L1235, L2250, L2894). |
| `--crate` filter | ✅ | Rust-additive | cfgd release.yml L83/117 uses `--crate <workspace>` across four runs. |
| depends_on (workspace ordering) | ✅ | Rust-additive | cfgd declares `depends_on: [cfgd-core]` → core releases first, others after. |

## CLI

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-project-global-cli.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| release / build / check / healthcheck / init / completion / jsonschema | ✅ | OSS | Comprehensive integration suite in `crates/cli/tests/integration.rs`. |
| changelog preview | ✅ | Pro | `commands/changelog.rs` + inline tests (dry-run command). |
| continue / publish / announce (composite Pro commands) | ✅ | Pro | Wired via `anodize release --merge` in cfgd release workflow. |
| tag (Rust-additive, auto-tag on master push) | ✅ | Rust-additive | anodize CI auto-tag step ran on every master push → produced v0.1.1–v0.2.5 tags live. |
| targets --json (Rust-additive) | ✅ | Rust-additive | Consumed by [anodize-action](https://github.com/tj-smith47/anodize-action/actions/runs/24409150253) as matrix input. |
| resolve-tag (Rust-additive, tag → crate mapping) | ✅ | Rust-additive | cfgd release.yml L36 uses `resolve-workspace: 'true'`; run `24442229349` resolved `core-v0.3.5` → `cfgd-core`. |
| man (clap_mangen man page generation) | ❌ | OSS | **Not implemented** — no `man.rs` module in `commands/`. Niche per inventory §5. |
| --auto-snapshot / --clean / --config / --draft / --fail-fast / --parallelism / --release-notes / --skip / --snapshot / --split / --timeout / --single-target / --strict / --workspace | ✅ | OSS | Flag coverage in integration tests (L208/224/240/386/417/438/1413/716/3322). |
| --nightly | ⚠ | Pro | Flag wired; no live nightly release run. |
| --draft | ⚠ | OSS | Flag wired; no live draft release (all production ships non-draft). |
| --prepare | ❌ | Pro | **Missing**. [known-bugs A1-rev #51](https://github.com/tj-smith47/anodize/blob/master/.claude/known-bugs.md). |
| --soft (non-fatal check) | ❌ | Pro | **Not implemented**. Niche per inventory §5. |

## Template helpers (Tera)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-template-helpers.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| Tera engine (Rust-native replacement for Go text/template) + GoReleaser-compatible `{{ .Field }}` syntax | ✅ | Rust-native | Every release asset name on v0.2.5 + cfgd v0.3.5 is template-rendered. |
| String / path / filter / version / env helpers | ✅ | OSS | `core/src/template.rs` unit tests. |
| Hash helpers (blake2b / 2s / 3, crc32, md5, sha1 / 224 / 256 / 384 / 512, sha3_*) | ✅ | OSS | `core/src/template.rs` unit tests. |
| File I/O (readFile / mustReadFile), data (list / map / indexOrDefault), encoding (mdv2escape / urlPathEscape), misc (time / englishJoin) | ✅ | OSS | `core/src/template.rs` unit tests. |
| Pro helpers (`in`, `reReplaceAll`), `.Now.Format` preprocessor | ✅ | Pro | `core/src/template.rs` pre-processor rewrite. |
| Full variable set (.ProjectName / .Version / .Tag / .Major / .Minor / .Patch / .Os / .Arch / ...) | ✅ | OSS | Used across cfgd + anodize configs live. |
| Pro variables (.PrefixedTag / .Artifacts / .Metadata / .Var / .IsMerging / .IsRelease) | ✅ | Pro | cfgd uses `.Var.*` + `.Artifacts` in docker_manifests live. |

## Pipeline outputs + tokens

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-artifacts-metadata-auth.md)

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| dist/artifacts.json (full artifact manifest) | ✅ | OSS | v0.2.5 ships `artifacts.json` (11652 bytes) as release asset. |
| dist/metadata.json (release metadata) | ✅ | OSS | v0.2.5 ships `metadata.json` (128 bytes) as release asset. |
| dist/config.yaml (effective config) | ✅ | OSS | `test_e2e_build_command_matches_goreleaser_pipeline_outputs` L3617 asserts all three exist. |
| GITHUB_TOKEN | ✅ | OSS | cfgd + anodize release workflows consume `secrets.GH_PAT` live. |
| GITLAB_TOKEN / GITEA_TOKEN | ⚠ | OSS | Wired; no live GitLab/Gitea release. |
| GORELEASER_FORCE_TOKEN (`ForceTokenKind` enum) | ✅ | OSS | `core/src/token.rs` unit tests; no live override in anodize/cfgd. |

## Rust-additive (features beyond GoReleaser)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-artifacts-metadata-auth.md)

These features have no GoReleaser analogue — they exist because Rust's toolchain and packaging ecosystem differ. Twelve tracked in inventory §3.

| Feature | Status | Tier | Evidence |
|---|---|---|---|
| publish.crates_io (dependency-aware ordering + index polling) | ✅ | Rust-additive | cfgd publishes four crates live 2026-04-15 with `depends_on` ordering. |
| binstall metadata (cargo-binstall compatibility) | ✅ | Rust-additive | cfgd `binstall.enabled: true` live. |
| Cargo workspace detection (multi-crate monorepo) | ✅ | Rust-additive | cfgd 4-workspace monorepo → 4 simultaneous releases on 2026-04-15. |
| version_sync (Cargo.toml ↔ tag) | ✅ | Rust-additive | Recent fix `ce3e396` shipped in v0.2.5. |
| SkipMemento (stage-level skip memoization) | ✅ | Rust-additive | Used by every stage via `ctx.should_skip()`; `commands/build.rs` uses it for binary-sign gating. |
| ConventionalFileName per-packager (nfpm v2.44 closure) | ✅ | Rust-additive | v0.2.5 nfpm assets match per-format conventions (`anodize_0.2.5_linux_amd64.deb` vs `anodize-0.2.5-1.src.rpm`). |
| run_parallel_chunks (shared parallelism helper) | ✅ | Rust-additive | Used in 10 stages; live-proven by parallelized fan-outs in cfgd multi-crate release. |
| targets --json subcommand | ✅ | Rust-additive | Consumed by anodize-action matrix strategy. |
| resolve-tag subcommand (tag → crate mapping) | ✅ | Rust-additive | cfgd release.yml uses on every tag push. |
| ANODIZE_CURRENT_TAG / ANODIZE_PREVIOUS_TAG env | ✅ | Rust-additive | CI auto-tag step; commit `010388fb` wired it. |
| tag_pre_hooks / tag_post_hooks (templated vars) | ✅ | Rust-additive | Run in anodize CI auto-tag step live. |
| UPX target-triple globs (replaces goos/goarch patterns) | ✅ | Rust-additive | v0.2.5 binaries UPX-compressed; config uses Rust triples. |

## anodize-action

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-action-inputs-outputs.md)

The GitHub Action wrapping anodize — [source repo](https://github.com/tj-smith47/anodize-action).

| Feature | Status | Evidence |
|---|---|---|
| from-artifact / artifact-run-id / artifact-workflow | ✅ | anodize release.yml L51 uses `from-artifact: anodize-linux`. |
| from-source / install-rust | ✅ | action CI run 24409150253. |
| install (zig / cargo-zigbuild / upx / nfpm / makeself / snapcraft / rpmbuild / cosign) | ✅ | All eight tools installed across cfgd's four workspace releases 2026-04-15. |
| args / gpg-private-key / docker-registry / docker-password | ✅ | cfgd release.yml passes all four live. |
| upload-dist / download-dist / install-only | ✅ | cfgd release.yml L83/117 uses upload-dist on split + download-dist on merge. |
| resolve-workspace | ✅ | cfgd release.yml L36 uses on every tag push → four workspace runs 2026-04-15. |

## Missing / niche (not dogfooded)

[Evidence file](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/evidence-negative-space.md)

| Feature | Status | Tier | Notes |
|---|---|---|---|
| `anodize man` (clap_mangen) | ❌ | OSS | Not implemented. Niche per inventory §5. |
| `--soft` flag on check | ❌ | Pro | Not implemented. Niche per inventory §5. |
| continue_on_error (per-stage, log-and-continue) | ❌ | OSS | Not implemented — at odds with anodize's fail-fast design. Niche per inventory §5. |

## Outstanding live-release gaps

These features have implementation + unit tests but cannot be flipped to ✅ without a live release the maintainer chooses to run. They are **not** blocked by missing code.

- **macOS native notarize** (`notarize.macos_native`, Pro) — needs Apple Developer ID cert on a macOS runner. See [HANDOFF D2](https://github.com/tj-smith47/anodize/blob/master/.claude/audits/2026-04-v0.x/HANDOFF.md).
- **Pro installers** (dmgs, msis, pkgs, nsis, app_bundles) — need a live release producing `.dmg`, `.msi`, `.pkg`, NSIS `.exe`, and `.app` artifacts on macOS/Windows runners with hdiutil/wixl/pkgbuild/makensis available.
- **11 of 13 announcer channels** (discord, slack, telegram, teams, mattermost, reddit, twitter, mastodon, bluesky, linkedin, opencollective, discourse) — need a release with the respective secrets configured.
- **Cloud blob / artifactory / fury / cloudsmith / uploads** — need cloud credentials configured in a release workflow. anodize + cfgd use only GitHub Releases + crates.io + ghcr.io + package-manager repositories.
- **AI changelog backends** (anthropic / openai / ollama) — need a release configured with `changelog.use: ai`.
- **Nightly** — need a scheduled workflow trigger to produce a date-stamped release.
- **GitLab + Gitea** releases — need a live project on each SCM.
- **Flatpak bundle** — need flatpak runtime + flathub config.
- **AUR / homebrew_casks** — need live AUR SSH key and cask publish flow.
- **Docker Hub description sync** — anodize/cfgd use ghcr.io; needs a Docker Hub-anchored release.
- **Remote `includes[].from_url`** — needs a config that pulls a remote include.
- **GitLab token / Gitea token / `GORELEASER_FORCE_TOKEN` override** — need live releases on GitLab/Gitea or with a forced token kind.

## Contributing verification

If you verify a feature flagged ⚠ or ❌ here with new proof (CI run, release artifact, or test), open a PR linking the evidence — this matrix will be updated.

## Methodology

- Parity target: [GoReleaser](https://goreleaser.com/) at OSS HEAD `f7e73e3` (fetched 2026-04-16). Pro reference: [goreleaser.com/pro/](https://goreleaser.com/pro/) + docs (fetched 2026-04-16).
- Evidence gathered 2026-04-16 by the `dogfooding-evidence` agent team.
- Absence of evidence is evidence of absence here. Rows stay ⚠ or ❌ until a real run lands.
