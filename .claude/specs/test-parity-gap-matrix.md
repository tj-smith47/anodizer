# Test Parity Gap Matrix: Anodize vs GoReleaser

**Refresh date:** 2026-04-18 (A1 inventory mapper rerun; 2026-04-16 baseline closures applied)
**Anodize baseline:** ~3176 tests across 26 crates (~179k LOC per `wc -l crates/*/src/**`, grown from 441 tests / 17.8k LOC since March baseline)
**GoReleaser baseline:** ~164 test files, ~44k lines of test code, est. 3000+ test cases (unchanged — pinned to GoReleaser HEAD `f7e73e3`)

> **How to read this file.** Upper section is the refreshed parity-status summary (partial + missing features with verification commands). The legacy Gap Matrix below (Section 4) enumerates test-case-level gaps; kept for reference but not required reading for audit waves.

---

## 1. Features with `parity_status` ∈ {partial, missing}

Source: Section 2 of `goreleaser-complete-feature-inventory.md`. Only rows with `ecosystem_relevance` in {required, strongly-suggested} are audit-driving; niche rows are informational.

| name | parity_status | ecosystem_relevance | verification command / test name | notes |
|------|---------------|---------------------|----------------------------------|-------|
| `goreleaser man` (man page generation) | missing | niche | `cargo run --bin anodize -- man --help` (currently errors — subcommand absent) | Nice-to-have; `clap_mangen` would be the implementation path. Not required. |
| `--soft` flag on `anodize check` | missing | niche | `cargo run --bin anodize -- check --soft` (currently errors — flag absent) | Pro feature; anodize check is strict by default. |
| `continue_on_error` per-stage | missing | niche | no stage currently surfaces `continue_on_error` as a config key | Anodize is fail-fast; would need per-stage opt-in. |
| `metadata.full_description.from_url` | partial | niche | `anodize check` against `metadata.full_description.from_url: ...` raises "`from_url` is not yet supported at metadata context time" (core/src/context.rs:754) | Inline and `from_file` paths work; `FromUrl` deferred. |
| `mcp registry` (MCP server manifest publish) | missing | niche | no anodize stage; new GoReleaser pipe at `internal/pipe/mcp/` | MCP registry still forming; no Rust demand signal surfaced. |

**No required or strongly-suggested CLI features are in the partial/missing set.** This matches the 2026-04-18 completion statement in the CLI inventory (`Completion achieved: yes`).

---

## 2. Behavioral verification commands (audit-wave reference)

For auditors A2/A3/A4 who need to spot-check behavioral parity rather than take the inventory's `implemented` at face value:

| area | anodize test entry point | goreleaser reference test |
|------|--------------------------|----------------------------|
| build | `cargo test -p anodize-stage-build` + `crates/cli/tests/integration.rs::test_e2e_build_command_matches_goreleaser_pipeline_outputs` | `internal/pipe/build/build_test.go`, `internal/builders/rust/build_test.go` |
| archive | `cargo test -p anodize-stage-archive` | `internal/pipe/archive/archive_test.go` |
| checksum | `cargo test -p anodize-stage-checksum` | `internal/pipe/checksums/checksums_test.go` |
| nfpm | `cargo test -p anodize-stage-nfpm` (incl. `filename.rs` per-packager tests) | `internal/pipe/nfpm/nfpm_test.go` |
| homebrew | `cargo test -p anodize-stage-publish --test homebrew_integration` | `internal/pipe/brew/brew_test.go`, `internal/pipe/cask/cask_test.go` |
| docker | `cargo test -p anodize-stage-docker` | `internal/pipe/docker/docker_test.go`, `internal/pipe/docker/v2/*_test.go`, `internal/pipe/docker/manifest_test.go`, `internal/pipe/dockerdigest/digest_test.go` |
| sign | `cargo test -p anodize-stage-sign` | `internal/pipe/sign/sign_test.go`, `sign_binary_test.go`, `sign_docker_test.go` |
| sbom | `cargo test -p anodize-stage-sbom` | `internal/pipe/sbom/sbom_test.go` |
| changelog | `cargo test -p anodize-stage-changelog` | `internal/pipe/changelog/changelog_test.go` |
| release | `cargo test -p anodize-stage-release` (with MockGitHubClient) | `internal/pipe/release/release_test.go` |
| announce | `cargo test -p anodize-stage-announce` (14 providers) | `internal/pipe/{discord,slack,telegram,teams,mattermost,smtp,reddit,twitter,mastodon,bluesky,linkedin,opencollective,discourse,webhook}/*_test.go` |
| publish | `cargo test -p anodize-stage-publish` (homebrew/scoop/chocolatey/winget/aur/krew/nix/artifactory/cloudsmith/dockerhub/crates_io) | `internal/pipe/{brew,cask,scoop,chocolatey,winget,aur,krew,nix,artifactory,cloudsmith,custompublishers}/*_test.go` |
| blob | `cargo test -p anodize-stage-blob` | `internal/pipe/blob/*_test.go` |
| notarize | `cargo test -p anodize-stage-notarize` | `internal/pipe/notary/*_test.go` |
| snapcraft / flatpak / makeself / srpm / upx / dmg / msi / pkg / nsis / appbundle / source / templatefiles | `cargo test -p anodize-stage-<name>` | `internal/pipe/<name>/*_test.go` |
| partial | `cargo test -p anodize --test partial` + `commands/continue_cmd.rs` tests | `internal/pipe/partial/partial_test.go` |
| templates | `cargo test -p anodize-core --test template` | `internal/tmpl/tmpl_test.go` |
| CLI E2E | `cargo test -p anodize --test integration` | `cmd/*_test.go` |

---

## 3. Structural test gaps (unchanged from March baseline — still open)

The following structural gaps from the March 2026 baseline remain — test infrastructure items, not feature parity gaps:

1. **No mock HTTP client trait for non-GitHub providers** — `MockGitHubClient` exists, but scoop/homebrew/winget/chocolatey repository push paths still shell out to real `git`.
2. **No real-subprocess `fakeBuilder`** — build tests verify command construction, not actual compile.
3. **No golden file testing** — formulae / manifests / PKGBUILD / nix derivations are structure-verified but not diffed against a golden reference.
4. **No dedicated defaults-propagation tests** — defaults are tested per-stage but no `defaults_test.rs` enumerates every field.
5. **No fuzz testing** — template engine and artifact registry lack fuzz harnesses.
6. **Only changelog uses real git repo** — other stages use in-memory context.

These are test-infrastructure investments, not parity gaps per the parity definition. Track under repo-health, not parity-wave.

---

## 4. Legacy detailed gap matrix (reference — unchanged since 2026-03-26)

_Preserved verbatim below for reference. New audit waves should use Section 1 + Section 2 above; this section is kept as an auditor reference for test-case-level gaps within implemented features._

## Methodology

GoReleaser's testing strategy was analyzed by reading test files across their major subsystems. Key patterns observed:

1. **Mock clients** -- GoReleaser uses `client.Mock` to avoid real API calls while still testing the full upload/release pipeline
2. **Real file I/O** -- Archive, checksum, nfpm tests create real files on disk, build real archives, and verify contents
3. **Real git repos** -- Changelog tests init actual git repos with commits and tags, then run the full pipe
4. **Defaults pipe** -- Dedicated tests that config defaults are filled in correctly when fields are omitted
5. **Golden files** -- Brew/Scoop/Nix formula output compared against golden reference files
6. **Table-driven tests** -- Exhaustive table-driven tests for all variations of config (e.g., Docker has ~30+ table cases)
7. **Skip/error boundary tests** -- Every pipe tests its Skip() and error paths explicitly
8. **Fuzz tests** -- Template engine and artifact registry have fuzz tests

---

## Gap Matrix

### 1. Core: Config Parsing (`core/config.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (52) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `project_name` | Y | - | - | - | 3 | test empty project_name fallback; test project_name with special characters (dots, @, hyphens) |
| `dist` | Y | - | - | - | 1 | test custom dist path; test dist path with spaces; test dist path creation when missing |
| `defaults.targets` | Y | - | - | - | 1 | test empty targets array vs omitted; test invalid target triple in defaults |
| `defaults.cross` | Y | - | - | - | 1 | test all CrossStrategy values parse correctly (cargo, zigbuild, cross, auto) individually |
| `defaults.flags` | Y | - | - | - | 0 | **test flags field parses and propagates to builds** |
| `defaults.archives` | Y | - | - | - | 1 | test default archive format propagates when per-crate is omitted; test format_overrides in defaults |
| `defaults.checksum` | Y | - | - | - | 2 | test default checksum algorithm propagation |
| `crates[].depends_on` | - | - | - | - | 0 | **test depends_on parses correctly; test circular dependency detection; test missing dependency name** |
| `crates[].tag_template` | Y | - | - | - | 1 | **test tag_template with Tera-native syntax; test tag_template rendering with all vars** |
| `builds[].copy_from` | - | - | - | - | 0 | **test copy_from config parsing; test copy_from resolves source binary path** |
| `builds[].env` | - | - | - | - | 0 | **test per-target env map parsing; test env map with nested target keys** |
| `archives: false` | Y | - | - | - | 2 | (covered) |
| `archive.name_template` | Y | - | - | - | 1 | test name_template rendering with all variables |
| `archive.format` | Y | - | - | - | 1 | test all valid formats (tar.gz, tar.xz, tar.zst, zip, binary); test invalid format string |
| `archive.format_overrides` | Y | - | - | - | 1 | test multiple overrides; test override for unknown OS |
| `archive.files` | Y | - | - | - | 1 | test glob patterns; test empty files array; test nonexistent file path |
| `archive.binaries` | - | - | - | - | 0 | **test binaries list config parsing** |
| `archive.wrap_in_directory` | Y | - | - | - | 1 | (covered) |
| `checksum.name_template` | Y | - | - | - | 1 | test name_template with template variables |
| `checksum.algorithm` | Y | - | - | - | 2 | test all algorithm strings parse (sha256, sha512, sha1, sha224, sha384, blake2b, blake2s); test invalid algorithm |
| `checksum.extra_files` | Y | - | - | - | 1 | (covered) |
| `checksum.ids` | Y | - | - | - | 1 | (covered) |
| `release.github` | Y | - | - | - | 1 | test github owner/name parsing |
| `release.draft` | Y | - | - | - | 1 | (covered) |
| `release.prerelease` | Y | - | Y | - | 2 | (covered -- auto/bool/invalid) |
| `release.make_latest` | Y | - | Y | - | 5 | (well covered) |
| `release.name_template` | Y | - | - | - | 1 | test template rendering in name_template |
| `release.header/footer` | Y | - | - | - | 2 | (covered) |
| `release.extra_files` | Y | - | - | - | 2 | (covered) |
| `release.skip_upload` | Y | - | - | - | 2 | (covered) |
| `release.replace_existing_draft` | Y | - | - | - | 1 | (covered) |
| `release.replace_existing_artifacts` | Y | - | - | - | 1 | (covered) |
| `publish.crates` (bool/object) | Y | - | - | - | 1 | (covered) |
| `publish.homebrew` | Y | - | - | - | 0 | **test homebrew tap config parsing; test homebrew fields (description, license, install, test)** |
| `publish.scoop` | Y | - | - | - | 0 | **test scoop bucket config parsing; test scoop fields (description, license)** |
| `docker.image_templates` | Y | - | - | - | 1 | test multiple image templates; test template rendering in image tags |
| `docker.dockerfile` | Y | - | - | - | 0 | **test dockerfile path parsing** |
| `docker.platforms` | Y | - | - | - | 1 | test custom platform list parsing |
| `docker.build_flag_templates` | Y | - | - | - | 0 | **test build_flag_templates parsing** |
| `docker.skip_push` | Y | - | - | - | 1 | (covered) |
| `docker.extra_files` | Y | - | - | - | 1 | (covered) |
| `docker.push_flags` | Y | - | - | - | 1 | (covered) |
| `nfpm.package_name` | Y | - | - | - | 1 | (covered) |
| `nfpm.formats` | Y | - | - | - | 1 | test multiple formats; test invalid format |
| `nfpm.contents` | Y | - | - | - | 3 | (well covered) |
| `nfpm.scripts` | Y | - | - | - | 1 | (covered) |
| `nfpm.dependencies` | Y | - | - | - | 1 | (covered) |
| `nfpm.file_name_template` | - | - | - | - | 0 | **test file_name_template parsing and rendering** |
| `nfpm.overrides` | - | - | - | - | 0 | **test format-specific overrides parsing** |
| `changelog.sort` | Y | - | - | - | 1 | (covered) |
| `changelog.filters` | Y | - | - | - | 2 | (covered) |
| `changelog.groups` | Y | - | - | - | 1 | (covered) |
| `changelog.header/footer` | Y | - | - | - | 1 | (covered) |
| `changelog.disable` | Y | - | - | - | 2 | (covered) |
| `changelog.use` | Y | - | - | - | 1 | (covered) |
| `changelog.abbrev` | Y | - | - | - | 1 | (covered) |
| `signs[]` (array/object) | Y | - | - | - | 5 | (well covered) |
| `sign.signature` | Y | - | - | - | 1 | test custom signature template |
| `sign.stdin/stdin_file` | Y | - | - | - | 1 | (covered) |
| `sign.ids` | Y | - | - | - | 1 | (covered) |
| `snapshot.name_template` | Y | - | - | - | 1 | (covered) |
| `announce.discord` | Y | - | - | - | 1 | (covered) |
| `announce.slack` | Y | - | - | - | 1 | (covered) |
| `announce.webhook` | Y | - | - | - | 1 | (covered) |
| `publishers[]` | - | - | - | - | 0 | **test publisher config parsing; test publisher with env, ids, artifact_types** |
| `before/after hooks` | Y | - | - | - | 0 | **test hooks config parsing (not just CLI execution)** |
| `env` (global) | Y | - | - | - | 3 | test env var template expansion; test env override behavior |
| Error: malformed YAML | - | - | Y | - | 1 | (covered) |
| Error: type mismatches | - | - | Y | - | 3 | (covered) |
| Error: unknown fields | - | - | Y | - | 3 | (covered) |
| Error: empty config | - | - | Y | - | 1 | (covered) |

### 2. Core: Template Engine (`core/template.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (33) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Go-style dot syntax | - | Y | - | - | 5 | (covered) |
| Tera-native syntax | - | Y | - | - | 2 | test more Tera-native expressions without dot prefix |
| `tolower` / `toupper` filters | - | Y | - | - | 2 | (covered in filter_chaining) |
| `trimprefix` filter | - | Y | - | - | 0 | **test trimprefix with prefix arg; test trimprefix when prefix not present** |
| `trimsuffix` filter | - | Y | - | - | 0 | **test trimsuffix with suffix arg; test trimsuffix when suffix not present** |
| `default` filter | - | Y | - | - | 1 | test default for empty string vs undefined |
| Conditionals (if/else) | - | Y | - | - | 2 | test nested conditionals; test falsy value semantics |
| Env variable access | - | Y | - | - | 1 | test missing env var behavior; test env var with special characters |
| Archive name template | - | Y | - | - | 1 | test archive name with all known variables (Os, Arch, Version, etc.) |
| Error: bad syntax | - | - | Y | - | 1 | (covered) |
| Error: invalid filter arg | - | - | Y | - | 1 | (covered) |
| Error: includes original template | - | - | Y | - | 1 | (covered) |
| **Missing: `incpatch` / `incminor` / `incmajor` functions** | - | - | - | - | 0 | **GoReleaser has version increment functions; test if they exist or should be added** |
| **Missing: `envOrDefault` function** | - | - | - | - | 0 | **GoReleaser supports `envOrDefault`; test or implement** |
| **Missing: `isEnvSet` function** | - | - | - | - | 0 | **GoReleaser supports `isEnvSet`; test or implement** |
| **Missing: `replace` filter** | - | - | - | - | 0 | **GoReleaser supports `replace`; Tera has it natively -- test** |
| **Missing: `base` / `dir` path functions** | - | - | - | - | 0 | **GoReleaser supports path functions; test or implement** |
| **Missing: `urlPathEscape`** | - | - | - | - | 0 | **GoReleaser supports URL escaping; test or implement** |
| **Missing: fuzz tests** | - | - | - | - | 0 | **GoReleaser has template fuzz tests** |

### 3. Core: Context (`core/context.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (14) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `populate_git_vars` | - | Y | - | - | 3 | test all 14+ variables are set (Tag, Version, RawVersion, Major, Minor, Patch, Prerelease, FullCommit, Commit, ShortCommit, PreviousTag, Branch, IsSnapshot, GitTreeState) |
| `populate_time_vars` | - | Y | - | - | 1 | test Timestamp, Date, Now variables are populated |
| `should_skip` | - | Y | - | - | 1 | test multiple skip stages; test empty skip list |
| `is_dry_run` | - | Y | - | - | 1 | (covered) |
| `is_snapshot` | - | Y | - | - | 1 | (covered) |
| `is_draft` | - | Y | - | - | 1 | (covered) |
| `changelogs` map | - | - | - | - | 0 | **test changelog storage and retrieval per crate** |
| `github_native_changelog` flag | - | - | - | - | 0 | **test flag propagation from changelog stage to release stage** |
| **Missing: context with real git repo** | - | - | - | - | 0 | **GoReleaser tests context population from real git repos** |

### 4. Core: Artifact Registry (`core/artifact.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (10) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `add` + `all` | - | Y | - | - | 1 | (covered) |
| `by_kind` filtering | - | Y | - | - | 1 | (covered) |
| `to_metadata_json` | - | Y | - | - | 2 | (covered) |
| `format_size` | - | Y | - | - | 4 | (covered) |
| `ArtifactKind` serialization | - | Y | - | - | 1 | (covered) |
| **Missing: filter by crate_name** | - | - | - | - | 0 | **test filtering artifacts by crate_name** |
| **Missing: filter by target** | - | - | - | - | 0 | **test filtering artifacts by target triple** |
| **Missing: filter by metadata key** | - | - | - | - | 0 | **test filtering artifacts by metadata (e.g., "id")** |
| **Missing: concurrent add safety** | - | - | - | - | 0 | **GoReleaser has fuzz tests for concurrent artifact operations** |
| **Missing: artifact deduplication** | - | - | - | - | 0 | **test adding duplicate artifacts** |

### 5. Core: Git (`core/git.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (10) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `parse_github_remote` | - | Y | Y | - | 5 | (well covered) |
| `parse_semver` | - | Y | - | - | 3 | test semver with build metadata; test semver with pre+build |
| `is_prerelease` | - | Y | - | - | 1 | test alpha, beta, dev, rc patterns individually |
| **Missing: git log parsing** | - | - | - | - | 0 | **test extracting commits between two tags** |
| **Missing: dirty tree detection** | - | - | - | - | 0 | **test detecting uncommitted changes in working tree** |
| **Missing: tag listing** | - | - | - | - | 0 | **test listing tags, finding previous tag** |
| **Missing: git operations with real repo** | - | - | - | - | 0 | **GoReleaser tests git ops against real temporary repos** |

### 6. Core: Target Mapping (`core/target.rs`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (4) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Darwin arm64 | - | Y | - | - | 1 | (covered) |
| Windows x86_64 | - | Y | - | - | 1 | (covered) |
| Linux x86_64 | - | - | - | - | 0 | **test x86_64-unknown-linux-gnu mapping** |
| Linux aarch64 | - | - | - | - | 0 | **test aarch64-unknown-linux-gnu mapping** |
| Linux musl variants | - | - | - | - | 0 | **test musl target mappings** |
| FreeBSD | - | - | - | - | 0 | **test FreeBSD target mappings** |
| Unknown target | - | - | Y | - | 1 | (covered) |
| **Missing: all common Rust triples** | - | - | - | - | 0 | **GoReleaser tests exhaustive target/arch combos** |

### 7. Stage: Build (`stage-build`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (10) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Cargo strategy | - | Y | - | - | 1 | (covered) |
| Zigbuild strategy | - | Y | - | - | 1 | (covered) |
| Cross strategy | - | Y | - | - | 1 | (covered) |
| Auto strategy detection | - | Y | - | - | 1 | (covered) |
| Features flag | - | Y | - | - | 1 | (covered) |
| No-default-features | - | Y | - | - | 1 | (covered) |
| Env vars | - | Y | - | - | 1 | (covered) |
| Empty targets skip | - | Y | - | - | 1 | (covered) |
| Invalid target triple | - | - | Y | - | 1 | (covered) |
| Empty binary name | - | - | Y | - | 1 | (covered) |
| **Missing: copy_from behavior** | - | - | - | - | 0 | **test copy_from copies binary instead of building; test copy_from with missing source** |
| **Missing: per-build target override** | - | - | - | - | 0 | **test that per-build targets override global defaults** |
| **Missing: flags propagation** | - | - | - | - | 0 | **test that global flags propagate when per-build flags omitted** |
| **Missing: profile detection** | - | - | - | - | 0 | **test --release flag sets profile to release; test debug profile without --release** |
| **Missing: windows .exe suffix** | - | - | - | - | 0 | **test Windows target gets .exe appended to binary name** |
| **Missing: artifact registration** | - | - | - | - | 0 | **test that build stage registers Binary artifacts with correct metadata (target, binary name, path)** |
| **Missing: dry-run behavior** | - | - | - | - | 0 | **test that dry-run logs command but does not execute** |
| **Missing: build failure error** | - | - | - | - | 0 | **test that non-zero exit code from cargo produces descriptive error; GoReleaser tests fakeFailedBuild** |
| **Missing: selected_crates filter** | - | - | - | - | 0 | **test that only selected crates are built when filter is set** |
| **Missing: before/after hooks around build** | - | - | - | - | 0 | **GoReleaser tests pre/post build hooks create files** |
| **Missing: actual build E2E** | - | - | - | Y | 0 | **GoReleaser creates real fake binaries and builds them; test with a real tiny crate** |

### 8. Stage: Archive (`stage-archive`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (24) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| tar.gz creation | - | Y | - | - | 1+1 | (covered with integration tests) |
| tar.xz creation | - | Y | - | - | 1+1 | (covered) |
| tar.zst creation | - | Y | - | - | 1+1 | (covered) |
| zip creation | - | Y | - | - | 1+1 | (covered) |
| wrap_in_directory | - | Y | - | - | 4 | (covered for all formats) |
| format_for_target overrides | - | Y | - | - | 1 | (covered) |
| binary format (no archive) | - | Y | - | - | 1 | (covered) |
| glob pattern resolution | - | Y | - | - | 2 | (covered) |
| copy binary single/multiple | - | Y | - | - | 2 | (covered) |
| Disabled stage | - | Y | - | - | 1 | (covered) |
| Integration realistic trees | - | - | - | Y | 5 | (good coverage) |
| **Missing: name_template rendering** | - | - | - | - | 0 | **test archive name_template with all variables (ProjectName, Version, Os, Arch)** |
| **Missing: format_overrides multiple** | - | - | - | - | 0 | **test multiple format_overrides (e.g., windows->zip, darwin->tar.gz)** |
| **Missing: archive with additional files** | - | - | - | - | 0 | **test that files: glob patterns are included alongside binaries in the archive** |
| **Missing: empty files list** | - | - | - | - | 0 | **test archive with no extra files (binaries only)** |
| **Missing: nonexistent files in glob** | - | - | - | - | 0 | **test behavior when glob matches nothing** |
| **Missing: archive stage registers artifacts** | - | - | - | - | 0 | **test that ArchiveStage.run() adds Archive artifacts to context** |
| **Missing: default archive config inheritance** | - | - | - | - | 0 | **test that defaults.archives.format applies when per-crate format is omitted** |
| **Missing: multiple binaries per archive** | - | - | - | - | 0 | **GoReleaser tests archives containing multiple binaries** |

### 9. Stage: Changelog (`stage-changelog`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (34) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Conventional commit parsing | - | Y | - | - | 4 | (covered) |
| Sort asc/desc | - | Y | - | - | 2 | (covered) |
| Exclude filters | - | Y | - | - | 2 | (covered) |
| Include filters | - | Y | - | - | 3 | (covered) |
| Groups | - | Y | - | - | 3 | (covered, including empty and "others" bucket) |
| Header/footer | - | Y | - | - | 3 | (covered) |
| Abbrev | - | Y | - | - | 3 | (covered) |
| Disabled stage | - | Y | - | - | 1 | (covered) |
| GitHub native | - | Y | - | - | 1 | (covered) |
| Real git repo integration | - | - | - | Y | 4 | (good coverage) |
| Render to markdown | - | Y | - | - | 4 | (covered) |
| Combined include+exclude | - | Y | - | - | 1 | (covered) |
| Config parsing fields | Y | - | - | - | 3 | (covered) |
| **Missing: changelog with merge commits** | - | - | - | - | 0 | **GoReleaser tests filtering merge pull request commits** |
| **Missing: changelog with special chars** | - | - | - | - | 0 | **GoReleaser tests commits with quotes, angle brackets, etc.** |
| **Missing: changelog written to CHANGELOG.md** | - | - | - | - | 0 | **GoReleaser verifies changelog file is written to dist** |
| **Missing: release notes from file** | - | - | - | - | 0 | **GoReleaser tests --release-notes flag overriding changelog** |
| **Missing: changelog for different SCMs** | - | - | - | - | 0 | **GoReleaser tests changelog for GitHub, GitLab, Gitea** |
| **Missing: empty changelog** | - | - | - | - | 0 | **test behavior when no commits between tags** |

### 10. Stage: Checksum (`stage-checksum`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (22) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| All hash algorithms | - | Y | - | - | 7 | (all algorithms covered) |
| Name template | - | Y | - | - | 1 | test name_template with env vars (GoReleaser tests `{{ .Env.FOO }}` in name) |
| Extra files | - | Y | - | - | 1 | (covered) |
| IDs filter | - | Y | - | - | 1 | (covered) |
| Global disable | - | Y | - | - | 1 | (covered) |
| Per-crate disable | - | Y | - | - | 1 | (covered) |
| Dry-run | - | Y | - | - | 1 | (covered) |
| No artifacts skip | - | Y | - | - | 1 | (covered) |
| Integration tests | - | - | - | Y | 3 | (good -- file format, hash verification, multi-algo) |
| Config parsing | Y | - | - | - | 2 | (covered) |
| **Missing: checksum file refresh on change** | - | - | - | - | 0 | **GoReleaser tests that modifying a file after checksumming refreshes the checksum** |
| **Missing: split checksums (per-artifact)** | - | - | - | - | 0 | **GoReleaser supports `split: true` for per-artifact checksum files** |
| **Missing: missing file error** | - | - | Y | - | 0 | **test error when artifact file does not exist on disk** |
| **Missing: checksum artifact registration** | - | - | - | - | 0 | **test that checksum file is registered as Checksum artifact** |

### 11. Stage: Docker (`stage-docker`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (14) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `build_docker_command` basic | - | Y | - | - | 1 | (covered) |
| Multiple tags | - | Y | - | - | 1 | (covered) |
| Skip push | - | Y | - | - | 1 | (covered) |
| Push flags | - | Y | - | - | 1 | (covered) |
| Platform mapping | - | Y | - | - | 2 | (covered) |
| Dry-run registers artifacts | - | Y | - | - | 1 | (covered) |
| Extra files staging | - | Y | - | - | 2 | (covered for dry-run and live) |
| Config new fields | Y | - | - | - | 2 | (covered) |
| Skips without config | - | Y | - | - | 1 | (covered) |
| **Missing: image_templates rendering** | - | - | - | - | 0 | **test image_templates with {{ .Version }}, {{ .Tag }} variables** |
| **Missing: multi-platform build** | - | - | - | - | 0 | **test multiple platforms generate correct --platform flag** |
| **Missing: build_flag_templates rendering** | - | - | - | - | 0 | **test build_flag_templates resolve template variables** |
| **Missing: binary staging per arch** | - | - | - | - | 0 | **test that binaries are correctly staged per architecture subdirectory** |
| **Missing: Dockerfile copying** | - | - | - | - | 0 | **test Dockerfile is copied to staging dir** |
| **Missing: docker manifest creation** | - | - | - | - | 0 | **GoReleaser has extensive manifest list tests** |
| **Missing: Docker label support** | - | - | - | - | 0 | **GoReleaser tests OCI labels on built images** |
| **Missing: skip_push=auto with prerelease** | - | - | - | - | 0 | **GoReleaser tests auto-skip on pre-release versions** |
| **Missing: error on docker not installed** | - | - | Y | - | 0 | **test graceful error when docker command not found** |
| **Missing: real docker build E2E** | - | - | - | Y | 0 | **GoReleaser has integration tests that run real docker builds against a local registry (~44KB test file)** |

### 12. Stage: Nfpm (`stage-nfpm`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (15) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| YAML generation | - | Y | - | - | 7 | (good coverage of various YAML fields) |
| Contents with type/file_info | Y | Y | - | - | 2 | (covered) |
| Package relationships | Y | - | - | - | 1 | (covered) |
| Scripts | Y | Y | - | - | 2 | (covered) |
| Command construction | - | Y | - | - | 2 | (covered) |
| Dry-run | - | Y | - | - | 1 | (covered) |
| Skips without config | - | Y | - | - | 1 | (covered) |
| **Missing: multiple formats** | - | - | - | - | 0 | **test generating deb + rpm in one pass** |
| **Missing: invalid format error** | - | - | Y | - | 0 | **GoReleaser tests error on unsupported format string** |
| **Missing: file_name_template rendering** | - | - | - | - | 0 | **test file_name_template with version/arch variables** |
| **Missing: format overrides** | - | - | - | - | 0 | **test format-specific overrides (e.g., rpm-specific fields)** |
| **Missing: contents glob expansion** | - | - | - | - | 0 | **test contents source with glob patterns** |
| **Missing: artifact registration** | - | - | - | - | 0 | **test that LinuxPackage artifacts are registered** |
| **Missing: bindir correctness** | - | - | - | - | 0 | **test that binaries are placed in correct bindir** |
| **Missing: real nfpm build E2E** | - | - | - | Y | 0 | **GoReleaser tests actual .deb/.rpm generation (~54KB test file)** |
| **Missing: template expressions in fields** | - | - | - | - | 0 | **GoReleaser tests template expressions in description, homepage, etc.** |

### 13. Stage: Publish (`stage-publish`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (26) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Homebrew formula generation | - | Y | - | - | 4 | (covered -- single, multi-arch, class name) |
| Homebrew integration | - | - | - | Y | 4 | (covered -- complete structure, multiline install, no archives) |
| Homebrew dry-run | - | Y | - | - | 1 | (covered) |
| Scoop manifest generation | - | Y | - | - | 2 | (covered) |
| Scoop integration | - | - | - | Y | 5 | (covered -- JSON structure, bin, autoupdate, special chars) |
| Scoop dry-run | - | Y | - | - | 1 | (covered) |
| Crates.io publish command | - | Y | - | - | 1 | (covered) |
| Crates.io topo sort | - | Y | - | - | 2 | (covered) |
| Stage routing | - | Y | - | - | 5 | (covered) |
| **Missing: Homebrew tap git push** | - | - | - | - | 0 | **GoReleaser tests actual git push to tap repo via mock client** |
| **Missing: Scoop bucket git push** | - | - | - | - | 0 | **GoReleaser tests actual git push to bucket repo via mock client** |
| **Missing: Crates.io publish with --token** | - | - | - | - | 0 | **test token is passed to cargo publish** |
| **Missing: Crates.io index_timeout behavior** | - | - | - | - | 0 | **test that index_timeout config is used in publish wait** |
| **Missing: formula with dependencies** | - | - | - | - | 0 | **GoReleaser tests formulae with depends_on, conflicts, caveats** |
| **Missing: formula with test block** | - | - | - | - | 0 | **test Homebrew test stanza in formula** |
| **Missing: formula with custom_block** | - | - | - | - | 0 | **GoReleaser tests custom_block insertion** |
| **Missing: golden file comparison** | - | - | - | - | 0 | **GoReleaser uses golden files for formula/manifest verification** |
| **Missing: skip on prerelease** | - | - | - | - | 0 | **GoReleaser tests auto-skip publish for pre-release versions** |

### 14. Stage: Release (`stage-release`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (33) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Prerelease detection | - | Y | - | - | 7 | (well covered -- auto, explicit, alpha, beta, dev, rc) |
| Release body building | - | Y | - | - | 7 | (well covered -- all combinations of header/footer/changelog) |
| Extra files collection | - | Y | - | - | 5 | (covered -- real file, no matches, no patterns, directories, invalid glob) |
| make_latest resolution | - | Y | - | - | 4 | (covered) |
| Missing token error | - | - | Y | - | 1 | (covered) |
| No github config skip | - | - | - | - | 1 | (covered) |
| Skip upload | - | Y | - | - | 1 | (covered) |
| Dry-run behavior | - | Y | - | - | 3 | (covered) |
| replace_existing defaults | - | Y | - | - | 2 | (covered) |
| Skips crate without config | - | Y | - | - | 1 | (covered) |
| **Missing: actual GitHub API release creation** | - | - | - | - | 0 | **GoReleaser tests full release pipeline with mock client (create release, upload files, verify uploaded names)** |
| **Missing: release with IDs filter** | - | - | - | - | 0 | **GoReleaser tests filtering uploaded artifacts by ID** |
| **Missing: replace_existing_draft behavior** | - | - | - | - | 0 | **test that existing draft is deleted before creating new one** |
| **Missing: replace_existing_artifacts behavior** | - | - | - | - | 0 | **test that existing release assets are replaced** |
| **Missing: release name_template rendering** | - | - | - | - | 0 | **test that release name uses rendered template** |
| **Missing: draft release creation** | - | - | - | - | 0 | **test that draft: true creates a draft release** |
| **Missing: artifact upload errors** | - | - | Y | - | 0 | **test error handling when upload fails** |
| **Missing: release with checksums + signatures** | - | - | - | - | 0 | **GoReleaser tests that checksums and signatures are uploaded** |

### 15. Stage: Sign (`stage-sign`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (10) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Artifact filter (all 7 modes) | - | Y | - | - | 7 | (well covered) |
| Arg resolution | - | Y | - | - | 1 | (covered) |
| Empty signs skip | - | Y | - | - | 2 | (covered) |
| **Missing: actual signing with GPG** | - | - | - | - | 0 | **GoReleaser tests real GPG signing with a test keyring; tests sign all, archive, binary, checksum, package modes end-to-end** |
| **Missing: sign with cosign** | - | - | - | - | 0 | **GoReleaser tests cosign-based signing** |
| **Missing: sign command not found error** | - | - | Y | - | 0 | **GoReleaser tests exec.ErrNotFound for invalid sign cmd** |
| **Missing: sign command exit non-zero** | - | - | Y | - | 0 | **GoReleaser tests sign failure error message** |
| **Missing: invalid signature template** | - | - | Y | - | 0 | **GoReleaser tests bad template in signature field** |
| **Missing: invalid args template** | - | - | Y | - | 0 | **GoReleaser tests bad template in args** |
| **Missing: stdin/stdin_file piping** | - | - | - | - | 0 | **test stdin content piped to sign process; test stdin_file read and piped** |
| **Missing: ids filter** | - | - | - | - | 0 | **test that sign.ids filters which artifacts get signed** |
| **Missing: docker sign** | - | - | - | - | 0 | **test docker_signs config triggers cosign on DockerImage artifacts** |
| **Missing: dry-run logging** | - | - | - | - | 0 | **test dry-run logs would-run command without executing** |
| **Missing: multiple sign configs** | - | - | - | - | 0 | **GoReleaser tests multiple sign configs each signing different artifact types** |
| **Missing: template vars in args** | - | - | - | - | 0 | **test {{ .Env.GPG_FINGERPRINT }} resolution in sign args** |

### 16. Stage: Announce (`stage-announce`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (11) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| Discord payload | - | Y | - | - | 1 | (covered) |
| Slack payload | - | Y | - | - | 1 | (covered) |
| Webhook body | - | Y | - | - | 1 | (covered) |
| Dry-run all 3 providers | - | Y | - | - | 3 | (covered) |
| Disabled providers skip | - | Y | - | - | 3 | (covered) |
| No config skip | - | Y | - | - | 1 | (covered) |
| Missing webhook_url error | - | - | Y | - | 1 | (covered) |
| **Missing: message_template rendering** | - | - | - | - | 0 | **test that {{ .ProjectName }} {{ .Tag }} resolves in message** |
| **Missing: custom headers for webhook** | - | - | - | - | 0 | **test webhook custom headers are sent** |
| **Missing: custom content_type** | - | - | - | - | 0 | **test webhook content_type override** |
| **Missing: template error in webhook_url** | - | - | Y | - | 0 | **test invalid template in webhook_url** |
| **Missing: skip on patch version** | - | - | - | - | 0 | **GoReleaser tests announce skip template (e.g., `{{gt .Patch 0}}`)** |
| **Missing: multiple providers fail independently** | - | - | Y | - | 0 | **GoReleaser tests that multiple announce failures are collected as multi-error** |

### 17. CLI: Commands & Integration (`cli`)

| Aspect | Config Parsing | Behavior | Error Paths | E2E | Current (82) | Missing Test Cases |
|--------|:---:|:---:|:---:|:---:|:---:|---|
| `check` valid config | - | - | - | Y | 1 | (covered) |
| `check` invalid config | - | - | Y | Y | 1 | (covered) |
| `check` with -f flag | - | - | - | Y | 3 | (covered) |
| `init` generates config | - | - | - | Y | 1 | (covered) |
| `--help` output | - | - | - | Y | 1 | (covered) |
| `--version` output | - | - | - | Y | 1 | (covered) |
| `release --help` timeout | - | - | - | Y | 1 | (covered) |
| `build --help` timeout | - | - | - | Y | 1 | (covered) |
| Timeout kills long process | - | - | - | Y | 1 | (covered) |
| Pipeline hooks | - | Y | - | Y | varies | (covered) |
| **Missing: `release --dry-run` E2E** | - | - | - | Y | 0 | **test full pipeline in dry-run mode end-to-end** |
| **Missing: `release --snapshot` E2E** | - | - | - | Y | 0 | **test snapshot mode produces correct version** |
| **Missing: `release --skip` E2E** | - | - | - | Y | 0 | **test skipping specific stages** |
| **Missing: `build` command E2E** | - | - | - | Y | 0 | **test build subcommand produces binary artifacts** |
| **Missing: `changelog` command E2E** | - | - | - | Y | 0 | **test changelog subcommand writes changelog** |
| **Missing: stage ordering** | - | - | - | Y | 0 | **test that stages execute in correct order (build -> archive -> checksum -> sign -> release)** |
| **Missing: before/after hooks E2E** | - | - | - | Y | 0 | **test that before hooks run before stages and after hooks run after** |
| **Missing: parallelism flag** | - | - | - | - | 0 | **test --parallelism flag affects concurrent builds** |
| **Missing: --single-target flag** | - | - | - | - | 0 | **test --single-target limits build to host triple** |
| **Missing: publisher subcommand** | - | - | - | - | 0 | **test publisher command invocation with custom publishers** |
| **Missing: healthcheck subcommand** | - | - | - | - | 0 | **test healthcheck detects missing tools** |
| **Missing: TOML config E2E** | - | - | - | Y | 0 | **test full pipeline with .anodize.toml config** |
| **Missing: config file search precedence** | - | - | - | - | 0 | **test that .anodize.yaml is found before anodize.yaml** |
| **Missing: multi-crate release E2E** | - | - | - | Y | 0 | **test releasing multiple crates with depends_on ordering** |
| **Missing: selected_crates filter E2E** | - | - | - | Y | 0 | **test releasing specific crates with --crate flag** |

---

## Summary Statistics

| Category | Tests Have | Tests Missing | Coverage % |
|----------|-----------|---------------|-----------|
| Config parsing | 52 | ~25 | ~67% |
| Template engine | 33 | ~15 | ~69% |
| Context | 14 | ~8 | ~64% |
| Artifact registry | 10 | ~5 | ~67% |
| Git | 10 | ~6 | ~62% |
| Target mapping | 4 | ~5 | ~44% |
| Build stage | 10 | ~11 | ~48% |
| Archive stage | 24 | ~8 | ~75% |
| Changelog stage | 34 | ~6 | ~85% |
| Checksum stage | 22 | ~4 | ~85% |
| Docker stage | 14 | ~9 | ~61% |
| Nfpm stage | 15 | ~8 | ~65% |
| Publish stage | 26 | ~8 | ~76% |
| Release stage | 33 | ~8 | ~80% |
| Sign stage | 10 | ~12 | ~45% |
| Announce stage | 11 | ~6 | ~65% |
| CLI E2E | 82 | ~15 | ~85% |
| **Total** | **441** | **~149** | **~75%** |

### Key Structural Gaps vs GoReleaser

1. **No mock client for API testing** -- GoReleaser's `client.Mock` enables testing the full release/publish pipeline without network calls. Anodize has no equivalent, leaving all API-touching code paths untested.
2. **No real-subprocess tests for build** -- GoReleaser uses a `fakeBuilder` to test the build pipeline without actually compiling. Anodize's build tests only verify command construction.
3. **No golden file testing** -- GoReleaser compares generated formulae/manifests against golden files. Anodize's integration tests verify structure but not exact output stability.
4. **No defaults propagation tests** -- GoReleaser has a dedicated `defaults_test.go` that verifies all config fields get correct defaults. Anodize lacks this.
5. **No fuzz testing** -- GoReleaser fuzzes templates and artifact operations. Anodize has none.
6. **No real git repo tests outside changelog** -- Only the changelog stage tests against real git repos. Build, release, and other stages that depend on git info are not tested with real repos.

---

## Top 30 Prioritized Missing Tests

Ranked by impact (bugs they would catch) and frequency (how often the code path runs in real usage).

| # | Priority | Module | Test Case | Why It Matters |
|---|----------|--------|-----------|----------------|
| 1 | **P0** | cli | Full `release --dry-run` E2E: init git repo, create config, run full pipeline, verify all stages execute and artifacts are registered | This is the single most important user workflow -- if this breaks, nothing works |
| 2 | **P0** | stage-build | Build stage artifact registration: verify Binary artifacts with correct path, target, crate_name, and metadata are added to context | Every downstream stage (archive, checksum, release) depends on this |
| 3 | **P0** | stage-build | Build failure error: test non-zero exit code produces clear error with crate name, binary, and target | Users see this error regularly; it must be helpful |
| 4 | **P0** | stage-release | Mock-client release pipeline: create release, upload artifacts, verify uploaded file names (requires introducing mock client) | Without this, the entire release stage is untested beyond data assembly |
| 5 | **P0** | stage-archive | Archive stage artifact registration: run ArchiveStage and verify Archive artifacts are registered in context | Archive artifacts feed into checksum and release stages |
| 6 | **P1** | stage-build | Dry-run logging: verify dry-run prints command without executing | Dry-run is the recommended first test for every release |
| 7 | **P1** | stage-build | Windows .exe suffix: verify binary gets .exe extension for windows targets | Silent failure would produce broken Windows releases |
| 8 | **P1** | stage-build | copy_from behavior: test binary copy instead of build, including missing source error | copy_from is the primary mechanism for multi-binary crates |
| 9 | **P1** | stage-build | Per-build target override: verify per-build targets override global defaults | Misconfigured targets produce wrong artifacts silently |
| 10 | **P1** | core/template | `trimprefix` and `trimsuffix` filters: test with and without matching prefix/suffix | These are used in name templates; silent failures corrupt artifact names |
| 11 | **P1** | stage-sign | Sign command not found error: test graceful error when gpg/cosign not installed | Users who forget to install gpg need a clear message |
| 12 | **P1** | stage-sign | Sign command failure: test non-zero exit code error reporting | Failed signing must not be silently swallowed |
| 13 | **P1** | stage-sign | Sign ids filter: test that only artifacts matching sign.ids are signed | Wrong filtering would sign wrong artifacts or miss them |
| 14 | **P1** | stage-sign | Dry-run logging: verify would-run command is printed | Users rely on dry-run to preview what will be signed |
| 15 | **P1** | cli | Stage ordering: verify stages execute in correct order | Wrong ordering (e.g., checksum before archive) produces broken releases |
| 16 | **P1** | stage-docker | Image template rendering: verify {{ .Version }}, {{ .Tag }} resolve in tags | Incorrect tags would push to wrong image references |
| 17 | **P1** | stage-docker | Docker not installed error: test graceful error message | Common first-time setup issue |
| 18 | **P1** | cli | `release --snapshot` E2E: test snapshot mode produces SNAPSHOT version | Snapshot mode is the primary CI testing workflow |
| 19 | **P2** | core/config | depends_on parsing: test basic parsing and circular dependency detection | Circular deps would cause infinite loops or deadlocks |
| 20 | **P2** | core/config | publisher config parsing: test cmd, args, ids, artifact_types, env | Publishers are untested at the config level |
| 21 | **P2** | core/config | hooks config parsing: test before/after hooks parse correctly | Hooks are only tested via CLI integration, not config unit tests |
| 22 | **P2** | stage-nfpm | Multiple formats in one pass: test deb + rpm generation | This is the standard use case |
| 23 | **P2** | stage-nfpm | Invalid format error: test error message for unsupported format | GoReleaser explicitly tests this |
| 24 | **P2** | stage-nfpm | Artifact registration: verify LinuxPackage artifacts registered | Downstream stages depend on this |
| 25 | **P2** | core/context | Full git variable population with real repo: init repo, tag, and verify all 14+ template variables | Context population drives every template in every stage |
| 26 | **P2** | stage-checksum | Missing file error: test error when artifact file not on disk | Race conditions or path errors need clear messaging |
| 27 | **P2** | stage-publish | Homebrew tap push via mock: test formula is committed/pushed | The actual publish path is completely untested |
| 28 | **P2** | stage-publish | Skip on prerelease: test auto-skip for pre-release versions | Publishing a pre-release to Homebrew would be a serious mistake |
| 29 | **P2** | core/target | Complete target mapping: test all common Rust triples (linux-gnu, linux-musl, darwin, windows, freebsd) | Incorrect OS/Arch mapping corrupts archive names |
| 30 | **P2** | stage-announce | Message template rendering: verify ProjectName and Tag resolve in announce messages | Silent template failures would send `{{ .Tag }}` literally in Discord/Slack |

---

## Recommended Test Infrastructure Additions

To close the biggest gaps, anodize needs:

1. **Mock HTTP Client trait** -- Abstract the GitHub/crates.io API behind a trait, with a mock implementation that records calls. This unblocks testing the entire release and publish pipeline without network access.

2. **Test context builder** -- A helper like GoReleaser's `testctx.WrapWithCfg()` that constructs a Context with config, git info, artifacts, and template vars in one call. Reduces test boilerplate from 20+ lines to 3.

3. **Golden file test helper** -- A function that compares generated output (Homebrew formula, Scoop manifest, nfpm YAML) against golden reference files, with `UPDATE_GOLDEN=1` env var for regeneration.

4. **Git test fixtures** -- A helper that creates a temporary git repo with configurable commits and tags, like GoReleaser's `testlib.GitInit/GitCommit/GitTag`.

5. **Fake builder** -- A build strategy that creates a fake binary file without running cargo, enabling full pipeline tests without compilation time.

---

## Completion statement (test-parity matrix)

Refreshed 2026-04-18.

- Rows audited: 5 total (partial + missing), all ecosystem_relevance = niche.
  - required: 0 rows partial or missing
  - strongly-suggested: 0 rows partial or missing
  - niche: 5 (`goreleaser man`, `--soft`, `continue_on_error`, `metadata.full_description.from_url`, `mcp registry`)
  - not-applicable: 0 (the structural-test items in §3 are infrastructure, not parity rows)
- Implemented / partial / missing breakdown (required + strongly-suggested only):
  - implemented: all
  - partial: 0
  - missing: 0

Completion achieved: **yes** — every audit-driving row (required + strongly-suggested) has `parity_status = implemented`. Remaining gaps are five niche rows (informational only) plus six structural-test-infrastructure items tracked separately in §3.

Rationale: All 11 blockers from the 2026-04-16 A5 countersign are closed at source (see §5.closures of the CLI inventory). Test-level parity is the natural next audit (A2/A3/A4 will exercise each implemented row against its GoReleaser equivalent); gaps surfaced during those audits feed back into known-bugs.md per the wave process.

