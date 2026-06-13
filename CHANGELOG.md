# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.1] - 2026-06-13

### Features

* ce8ce6034072 consolidated run helper with verbose live-stream + emit-on-failure ([@tj-smith47](https://github.com/tj-smith47))
* 5a32ab7bf996 route per-crate no-config skips to debug; add --show-skipped ([@tj-smith47](https://github.com/tj-smith47))
* 75da7615ca30 proactive GitHub upload pace + secondary-RL exhaustion proof ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 784d18178ddb ship a runnable musl binary in the apk package ([@tj-smith47](https://github.com/tj-smith47))
* 52869be8b97d kill recursive sidecar chains via primary-subject taxonomy ([@tj-smith47](https://github.com/tj-smith47))
* bebbde927855 bind every build-consuming Linux surface to the gnu build ([@tj-smith47](https://github.com/tj-smith47))
* 1be521c3eb4f route live tee to stderr, concurrent stdin, dedup stream methods ([@tj-smith47](https://github.com/tj-smith47))
* 5d3a37425a54 sign by digest, never by a movable tag ([@tj-smith47](https://github.com/tj-smith47))
* 82639a0428da suppress false nothing-pushed warning on cask-only configs ([@tj-smith47](https://github.com/tj-smith47))
* 06e33eb62c64 make derivation formatting mandatory and fail loud, no unformatted push ([@tj-smith47](https://github.com/tj-smith47))
* 1a590e37ce4d parity — artifacts:all signs the combined checksums file ([@tj-smith47](https://github.com/tj-smith47))
* 6f77bf4ae887 restore GR parity — sign every Checksum kind, not combined-only ([@tj-smith47](https://github.com/tj-smith47))
* 28d100d1c270 surface verify-release findings in the end-of-release Summary ([@tj-smith47](https://github.com/tj-smith47))
* cda1eb7bd14d label container-start failures and anchor the smoke marker ([@tj-smith47](https://github.com/tj-smith47))
* 52e55ee29d64 make install smoke-test failures diagnosable ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* cd87602f30ed document recursion detector's name-suffix assumption ([@tj-smith47](https://github.com/tj-smith47))
* 9786400ff5a3 drop vestigial VerifyReleaseSummary.ran field ([@tj-smith47](https://github.com/tj-smith47))
* a08507ceafa3 pin Debug-verbosity cell of no-config skip matrix ([@tj-smith47](https://github.com/tj-smith47))
* 003776f7eb45 make upload_pace_zero_is_a_no_op deterministic (relative pacing compare) ([@tj-smith47](https://github.com/tj-smith47))
* 2fc21f14abe4 cover docker-sign digest-pin edge cases and the missing-digest path ([@tj-smith47](https://github.com/tj-smith47))
* f3761b63cdbf fix Windows fake-cosign arg capture in docker-sign digest test ([@tj-smith47](https://github.com/tj-smith47))

## [0.9.0] - 2026-06-11

### Bug Fixes

* 9ae4ed392be8 inject Is* template vars and NightlyBuild as typed bools/number (TJ Smith)
* decfe86b6618 review fixes — unset eviction, NightlyBuild truthiness note, test typing fidelity (TJ Smith)
* a6be5873b787 always write run summary; gate rollback on publish state (TJ Smith)
* ad49ce3fb5c7 review fixes — summary clobber guard, probe fail-closed, kms PATH seam (TJ Smith)
* 859e2f5860cf reject multi-document typed configs in builtin mode too (TJ Smith)
* bc2e553ead8c stop replacing PATH wholesale in spawn-failure tests (TJ Smith)
* fb7e5a166ad1 fail on missing expected signature/SBOM assets (TJ Smith)
* 9157a4919ff1 pin install-smoke containers to the package arch; drop apk self-provides (TJ Smith)
* 7634ac80354a re-review fixes — transitive ids verdict for derived subjects, typed multi-doc pin, docker_signs warning (TJ Smith)
* f79b7ebc5089 review fixes — resolved-name filter keying, probe pinning, probe diagnostics, lock recovery (TJ Smith)
* 727284f7957e review fixes — sbom derivation equivalence, release.ids sig inheritance (TJ Smith)

---
### Others

* cf02d17fcae2 rollback v0.8.0 [skip ci] (anodize-rollback)
* b73a16855234 "chore(release): rollback v0.8.0 [skip ci]" (TJ Smith)

## [0.8.0] - 2026-06-11

### Features

* 6a628185b1ba add --allow-rerun flag to anodizer publish (TJ Smith)
* cece142186c7 config-declarable on_error/on_rollback hooks (TJ Smith)
* 9e27a01471c9 expose failure-hook context as ANODIZER_* env vars (TJ Smith)
* 3eca978cea09 gate all irreversible publishers on any required failure (TJ Smith)
* 8f17ca07b47d preflight guard for publish-set dependency completeness (TJ Smith)
* e1c4c083e2de retain_on_rollback, on_error hooks, anodizer notify (TJ Smith)
* bdef8a2f957c universal publisher idempotency for safe re-runs (TJ Smith)
* a6665ddb2daf explicit --version override for autotag (TJ Smith)
* ca8155a6fef5 auto-detect dind for install_smoke; wire blobs to MinIO (TJ Smith)

---
### Bug Fixes

* c4031cd1a757 set ArtifactName from archive filename in url_template render (TJ Smith)
* 6e2b2387e422 unique SSH key filename per clone to prevent EEXIST on retry (TJ Smith)
* 39523204f1fb gate zigbuild routing on a reachable zig toolchain (TJ Smith)
* 173d8b4318b4 route host linux-gnu builds through zigbuild for a hermetic glibc floor (TJ Smith)
* ecc74f794f74 make republish_in_moderation actually re-push (TJ Smith)
* 3beec78e7866 add on_error to PublishDefaults with append-merge semantics; wire retain_on_rollback on cargo, schemastore, mcp (TJ Smith)
* 83da4f3add02 correct submitter required-gating warning text (TJ Smith)
* c0e19c2df6da durability fixes W1-W3, F1-F2, S1-S2, F3, GHA#1-2, #58-59 (TJ Smith)
* 532096abd458 error loudly when artifact URL absent in publish mode; tolerate in snapshot (TJ Smith)
* a29bb53a6a91 guarantee trailing newline on written SSH key (TJ Smith)
* cef07f0a4861 key the workspace-root dep cache by resolved root path (TJ Smith)
* 6e1980a40a5e propagate render errors in AUR rollback creds + add render tests (TJ Smith)
* 7e63afe5b5c5 redact custom header values and target URL in artifactory dry-run log (TJ Smith)
* 2a22c30de1f9 rehydrate sha256 via ChecksumStage in publish/continue pipeline (TJ Smith)
* 4de628a767e3 render npm registry/tag/metadata and dockerhub username templates (TJ Smith)
* 84f84dd40efd render secret/url/branch/token config templates before use (TJ Smith)
* e236fb1782ac resolve package renames in publish-set preflight (TJ Smith)
* 17bc6953a4dc resolve renames in the dependency wait gate (TJ Smith)
* 343d4f6c2a2b require all live publishers and restore install_smoke (TJ Smith)

---
### Others

* 4bd2c68379cd correct on_error timing and RolledBack semantics (TJ Smith)
* 099797b072ae DepEntry struct, alias in guard errors, shared root cache (TJ Smith)
* e1f5b88d14bd single-source failure vars; pin env-channel exhaustiveness (TJ Smith)
* 5c24cb2dfdc7 normalize hook output path for Windows in on_error test (TJ Smith)
* b9ad18ff3708 verify retain_on_rollback skips rollback dispatch (TJ Smith)
* 14d788e6014c test+fix: address v0.8.0 review findings (TJ Smith)

## [0.6.0] - 2026-06-08

Changes since `v0.5.0`. Will be cut as the next release.

### Features

- **`anodize tag rollback`** — new subcommand that deletes anodize-managed
  tags at a SHA and reverts (or resets past) the bump commit they point at.
  Failure-recovery counterpart to `anodize tag`. Flags: `--dry-run`,
  `--no-push`, `--scope={all,lockstep,per-crate}`,
  `--mode={revert,reset}`, `--branch <name>`. SHA-derived branch
  resolution is race-immune to default-branch movement. Safety check
  hard-fails when non-bump commits sit between HEAD and the target SHA
  under `--mode=revert`. (`3a27f92`, `5948253`, `ba81b6e`, `41947cb`)
- **`docker_v2:` graduates to canonical Docker API** with full GoReleaser
  v2.16 surface — Platforms metadata, pre/post hook contract
  (`{{ .Images }}` / `{{ .Dockerfile }}` / `{{ .ContextDir }}` /
  `{{ .Digest }}` / `{{ .BaseImage[Digest] }}`), podman backend (Linux-only),
  cleaner `images` + `tags` separation. Legacy `dockers:` block is now
  rejected at config-load time with a migration error. (`166e3a7`,
  `9e6f452`, `dbc87b7`)
- **Anodizer publishes itself as an MCP server.** The repo's own
  `.anodizer.yaml` declares `mcp:` + per-crate `docker_v2:`; the
  distroless OCI image at `ghcr.io/tj-smith47/anodizer:<version>` carries
  `ENTRYPOINT ["/usr/local/bin/anodizer"]` + `CMD ["mcp"]` so MCP clients
  `docker run` it as a stdio server. (`596e1a3`, `41947cb`)
- **Per-crate workspace-aware tag** — `anodizer tag` dispatches per-crate
  in workspaces with per-crate `[package].version`, emits `crates` (JSON
  array) and `versions` (JSON object) step outputs, propagates bumps to
  intra-workspace `path + version` dep specs. (`7735448`, `475109e`,
  `ba82aa1`, `0135f56`)
- **Per-crate dist subdir layout for workspace release** —
  `release --publish-only` consumes `preserved-dist/<crate>/` subdirs
  emitted by per-crate determinism shards. (`9c13daf`, `76cb613`,
  `9562bc3`)
- **`publishers[].required:` field** — every publisher accepts a
  `required:` boolean that wires through `resolved_required()` so the
  release pipeline knows whether a publisher's failure should block the
  Submitter gate / non-zero exit. Submitter-group publishers (cargo,
  chocolatey, winget, snapcraft, upstream-AUR) warn loudly when set to
  `true` since their failure cannot be recovered. (`a90f8ac`, `948dd4a`,
  `7de69a4`, `d035aaf`)
- **`if:` template-conditional gates** across publishers, hooks,
  announcers, archives, blob entries — when the rendered result is falsy
  (`"false"` / `"0"` / `"no"` / empty), the entry is skipped. Render
  failure hard-errors. (`10af9cf`)
- **AI release-note enhancement** — `changelog.use: ai` wires
  anthropic / openai / ollama as backends; produces a polished release
  note from the raw commit log. (`c8342c5`)
- **GoReleaser v2.16 parity** — nightly `tag_name:` templates, srpm
  Format/Ext overrides, immutable releases policy, `homebrew_casks:` as
  the canonical Homebrew surface (deprecated `publish.homebrew`),
  v2.12.6→v2.15.3 deprecation aliases for renamed fields. (`f9ec8d5`,
  `63bc5fc`, `1868af6`, `d0aff91`)
- **Pre-publishing hooks** (`before_publish:`) and per-artifact iteration
  with `ids` / `artifacts` filters. (`2e55c3f`, `a94ab91`)
- **Recursive config includes**, strict `template_vars:`, `meta_`
  propagation. (`42eb1ff`)
- **npm + gemfury publishers** — full implementation with idempotency
  probe, retry, templated extra files, rollback (npm) and `furies:`
  alias (gemfury). (`e3d7264`, `94e139d`, `2335dae`)
- **Single-target build, split/merge, nightly builds** audit closures —
  scheduled nightly workflow, version_template, keep_single_release with
  safety + dry-run visibility. (`aa11201`, `bc35263`, `b314c59`,
  `35e8d31`)
- **Per-publisher Pre-image SHA tracking** for `KrewExtra::bot_template_pre_image_shas`
  — rollback drift-detection for krew bot-template mode (Unchanged /
  Drifted / Missing / Unreadable). (`5948253`)
- **`actions: read` permission** required when the release job downloads
  artifacts from a sibling workflow (`from-artifact: anodizer-linux`,
  cross-workflow patterns). (workflow hardening)

### Fixes

- **Unblock cfgd release** — `--publish-only` resume_release auto-enable,
  per-iteration skip_stages propagation from `workspaces[].skip`,
  per-crate manifest path re-anchor, OCI `version` field omitted from the
  wire, BotTemplate pre-image SHA recording, cargo intra-workspace dep
  pin propagation. (`596e1a3`, `58b4e7a`, `76e766f`, `aec8eef`,
  `7f26c9f`, `6ca21a9`)
- **Source archive: extra-files mode normalized to 0o644 under SDE** for
  cross-OS determinism. (`c224627`)
- **Tag bump commits omit `[skip ci]` on the primary commit** so the
  tag-push trigger fires downstream `release.yml`. Side-effect
  `version_sync` propagation commits still carry the marker. (`a4d55d5`)
- **`wait_for_workspace_deps` gate** prevents cross-crate publish race
  during topo-ordered workspace publish. (`f756834`)
- **Detached-HEAD push** — `git push HEAD via refs/heads/<branch>`
  refspec, resolve detached HEAD before push. (`292af2d`, `68de654`)
- **Per-crate bump idempotent** when manifests are already at the target
  version. (`0135f56`)
- **`.anodizer.yaml workspaces:` takes precedence** over
  `[workspace.package].version` — authoritative signal for
  per-crate-with-grouping intent. (`e6a9ee9`)
- **`check`: fall back to `GITHUB_REF_NAME`** for tag_override when
  triggered by tag push. (`4b8d5c8`)
- **Audit follow-ups** drained across B1–B24 — pkg, msi, nsis, dmg,
  appbundle, changelog, build, release, git, docker, publish modules.

### CI / Workflows

- **Switched to cargo-nextest + sccache** layered atop rust-cache for
  faster CI. (`7d5573e`)
- **Scheduled nightly workflow** with date-based versioning. (`35e8d31`)
- **Sharded determinism matrix** (Linux + macOS + Windows-x86_64 +
  Windows-aarch64) — each shard validates only its own targets;
  cross-shard hash comparison is intentionally relaxed.
- **`Rollback on release failure` step** — workflow integrates
  `anodizer tag rollback "$GITHUB_SHA"` as the
  `if: (failure() || cancelled())` recovery hook.

### Docs

- New [Release Workflow Strategies](docs/site/content/docs/ci/release-workflows.md)
  page covering single-crate / lockstep / per-crate / hybrid / split-CI
  shapes with the decision tree. (`ef17e7d`)
- New `## crates[].docker_v2`, `## crates[].publish.krew`, `## mcp`
  schema sections in the auto-generated configuration reference.
- `tag rollback` documented in README + release-resilience guide +
  auto-tagging guide.
- `_preserved-bin/` layout documented in the determinism guide.
- `docker_v2:` page rewritten end-to-end; legacy `dockers:` references
  removed across packages, retry, dogfooding, and CI docs.
- MCP registry page: "Wiring the OCI image" subsection added.
- Krew page: "Rollback semantics for bot-template mode" + graceful
  degradation note for `project_root` auto-detect.
- anodizer-action page: 7 previously-undocumented inputs added
  (`apk-private-key`, `preserve-dist`, `shard-label`, `determinism`,
  `determinism-runs`, `determinism-stages`, `determinism-targets`);
  retry behavior callout updated to flag stateful
  `--publish-only` / `--rollback-only` / `tag rollback`.

[Unreleased]: https://github.com/tj-smith47/anodizer/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/tj-smith47/anodizer/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/tj-smith47/anodizer/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/tj-smith47/anodizer/compare/v0.6.0...v0.8.0
[0.6.0]: https://github.com/tj-smith47/anodizer/compare/v0.5.0...v0.6.0
