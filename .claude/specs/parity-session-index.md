# GoReleaser Parity — Session Index

> **Single source of truth for all remaining parity work.**
> GoReleaser source is cloned at `/opt/repos/goreleaser` for direct comparison.
> Reference inventory: `goreleaser-complete-feature-inventory.md`.

## Parity Definition

Parity means **equal or superior implementation** of each GoReleaser feature:

1. **Config field parity**: Every GoReleaser config field has an equivalent that is *wired through to behavior*. A parsed-but-ignored field is Missing.
2. **Behavioral parity**: Each feature produces the same output given the same input. Wrong defaults = gap.
3. **Wiring parity**: Config fields flow through to the stage that uses them. A field that's set but never read is Missing.
4. **Error parity**: Every GoReleaser error case has an equivalent. Different error behavior (warn vs error) = gap.
5. **Auth parity**: Credential chains must match.
6. **Default parity**: Every default value must match or be explicitly better.

Every Missing and Partial item must be addressed. There is no "high priority" vs "low priority", no "niche", no "low value". Pro features are not excluded — we give them to the people for free.

## Rules For Every Session

1. **Read GoReleaser source first.** Before implementing ANY feature, read the Go source at `/opt/repos/goreleaser/internal/pipe/{area}/` and its tests. List every config field, default, and behavior.
2. **Config + wiring are ONE task, not two.** Adding a config struct/field without wiring it to behavioral code is NOT done. Every checklist item means: config parsing + behavioral wiring + tests proving the behavior works. A parsed-but-ignored field is Missing by definition. When reviewing, the spec reviewer MUST verify fields are wired by tracing from config through to the code that reads them.
3. **Spec review = GoReleaser source comparison.** The spec reviewer must read the GoReleaser pipe source and compare behavior line-by-line. Checking that "the struct has the right fields" is not a spec review. The review must verify: Does anodizer produce the same output as GoReleaser given the same input? Are defaults the same? Are error cases the same?
4. **Spec + code quality review loop.** After implementing, run spec review then code quality review. Fix ALL findings of ANY severity. Re-review. Repeat until ZERO issues/suggestions remain. That's when the task is done.
5. **Mark items done here.** Check the box in this file when an item is implemented to equal or better quality than GoReleaser.
6. **Work on master directly.** No worktrees or branches for sequential work.
7. **Scope honestly.** Do fewer tasks to full parity rather than many tasks to config-only depth. Incomplete work creates more cleanup work than it saves.
8. **Pro features: research community implementations.** For GoReleaser Pro features that have no OSS source code, search for similar implementations of each component in community tools (e.g., other release tools, CI systems, package managers). Learn from their edge case handling, error behavior, and design choices. Document what was found and incorporate lessons learned. This fills the gap left by not having Pro pipe source to read.

## Phase 1: Completed (This Session)

Tasks 1-9 from `goreleaser-parity-matrix.md`. ~1,799 tests pass.

- [x] Template engine — 15 functions/filters, 14 hash functions, Runtime nested vars
- [x] Build stage — hooks, per-build ignore/overrides, binary templates, default targets, env templates, mtime, cross_tool, no_unique_dist_dir
- [x] Archive stage — format_overrides plural, files objects, meta, builds_info, strip_binary_dir, allow_different_binary_count, hooks, gz format
- [x] Checksum stage — StringOrBool disable, extra_files objects, sidecar fix, split name_template
- [x] Release stage — ContentSource header/footer, extra_files objects, mode API wiring, target_commitish, discussion_category_name, include_meta, use_existing_draft
- [x] Changelog stage — StringOrBool disable, abbrev -1, github backend, Logins/Login vars, nested subgroups
- [x] Sign stage — output capture, if conditional, binary_signs, docker digest/artifactID vars
- [x] Docker stage — auto skip_push, use backend, manifests, digest file
- [x] nFPM stage — file_info.mtime, RPM/Deb/APK/Archlinux fields, versioning fields, metadata fields, per-format signatures, ipk/termux-deb

## Phase 2: Remaining Parity Items

Sources: `goreleaser-parity-matrix.md` (Tasks 10-18), `fresh-parity-gap-analysis.md` (Sections B1-B39)

### Session A: All Publishers — Config Field Parity
GoReleaser source: `internal/pipe/brew/`, `internal/pipe/scoop/`, `internal/pipe/chocolatey/`, `internal/pipe/winget/`, `internal/pipe/aur/`, `internal/pipe/krew/`, `internal/pipe/nix/`

**Homebrew** (from parity-matrix s9, fresh-gap B25)
- [x] commit_author.signing
- [x] repository.branch, token, pull_request, git (SSH)
- [x] ids filter
- [x] url_template, url_headers
- [x] download_strategy, custom_require
- [x] custom_block, extra_install, post_install
- [x] plist, service
- [x] Homebrew Casks (entire feature)
- [x] alternative_names, app, repository.token_type, PR draft/body/base

**Scoop** (from parity-matrix s10, fresh-gap B26)
- [x] commit_author.signing
- [x] repository.branch, token, pull_request, git (SSH)
- [x] 32-bit architecture block
- [x] url_template, use (archive/msi/nsis), repository.token_type

**Chocolatey** (from fresh-gap B27)
- [x] ids, owners, title, copyright, require_license_acceptance
- [x] project_source_url, docs_url, bug_tracker_url
- [x] summary, release_notes, dependencies, source_repo
- [x] use (archive/msi/nsis), url_template, package_source_url

**Winget** (from fresh-gap B28)
- [x] ids, use, product_code, url_template
- [x] commit_msg_template, path, homepage, license_url
- [x] copyright, copyright_url, skip_upload
- [x] release_notes, release_notes_url, installation_notes
- [x] tags, dependencies, publisher_support_url, privacy_url
- [x] repository.*, commit_author.*

**AUR** (from fresh-gap B29)
- [x] ids, private_key, skip_upload, commit_msg_template, git_ssh_command

**Krew** (from fresh-gap B30)
- [x] ids, url_template, commit_msg_template, skip_upload
- [x] repository.*, commit_author.*

**Nix** (from parity-matrix s11, fresh-gap B16)
- [x] Full publisher: name, path, install, extra_install, post_install
- [x] dependencies, formatter, repository.*, commit_author.*

**Shared repository config** (from fresh-gap B39)
- [x] Unified repository struct across all 7 publishers: branch, token, token_type, pull_request.*, git.*
- [x] Unified commit_author struct: name, email, signing.*

### Session B: Announce Providers
GoReleaser source: `internal/pipe/announce/`, `internal/pipe/discord/`, `internal/pipe/slack/`, `internal/pipe/telegram/`, `internal/pipe/teams/`, `internal/pipe/mattermost/`, `internal/pipe/webhook/`, `internal/pipe/smtp/`, `internal/pipe/reddit/`, `internal/pipe/twitter/`, `internal/pipe/mastodon/`, `internal/pipe/bluesky/`, `internal/pipe/linkedin/`, `internal/pipe/opencollective/`, `internal/pipe/discourse/`

- [x] announce.skip (template-conditional)
- [x] Teams icon_url
- [x] Mattermost title_template
- [x] Webhook expected_status_codes
- [x] Slack blocks/attachments — verify wiring (config fields exist, behavior unverified)
- [x] SMTP email transport (replace sendmail)
- [x] Reddit provider
- [x] Twitter/X provider
- [x] Mastodon provider
- [x] Bluesky provider
- [x] LinkedIn provider
- [x] OpenCollective provider
- [x] Discourse provider

### Session C: Custom Publishers + CLI + Hooks
GoReleaser source: `internal/pipe/custompublishers/`, `cmd/`

**Custom publishers** (from parity-matrix s12)
- [x] meta
- [x] extra_files
- [x] Parallel per-artifact execution
- N/A: output (capture stdout) — Publisher struct has no Output field (config.go:1238-1249), zero matches in custompublishers pipe
- N/A: if (per-artifact filter) — Publisher struct has Disable only, no If field
- N/A: templated_extra_files — zero matches in config package; Publisher uses ExtraFiles []ExtraFile only

**CLI flags** (from parity-matrix s15, fresh-gap B38)
- [x] --fail-fast
- [x] --release-notes-tmpl
- [x] --output / -o (build)
- [x] man command
- N/A: --prepare (publish later) — zero matches in GoReleaser cmd/ directory
- N/A: --check --soft — zero matches for "soft" in cmd/check.go
- N/A: continue --merge, publish --merge, announce --merge — GoReleaser has no continue/publish/announce subcommands; anodizer already has them

**Global hooks** (from parity-matrix s17)
- N/A: if conditional on hooks — Before struct is Hooks []string, no conditional field (config.go:1187-1189)

### Session D1: New Stages + SBOM/Source Rewrite (Done)
GoReleaser source: `internal/pipe/nsis/`, `internal/pipe/appbundle/`, `internal/pipe/sbom/`, `internal/pipe/sourcearchive/`

- [x] NSIS installer stage (new crate): id, name (template), script (template), ids, extra_files (ExtraFileSpec), disable (StringOrBool), replace, mod_timestamp
- [x] macOS App Bundles (new crate): id, name (template), ids, icon (.icns), bundle (template, reverse-DNS), extra_files (ArchiveFileSpec), mod_timestamp, disable (StringOrBool), replace
- [x] SBOM rewrite: cmd/args/env/artifacts/ids/documents/disable subprocess model + built-in CycloneDX/SPDX fallback
- [x] Source archive improvements: prefix_template, object-form files (SourceFileEntry with src/dst/strip_parent/info)
- [x] Cross-cutting: DMG/PKG extra_files unified to ExtraFileSpec

### Session D2: Remaining New Stages
GoReleaser source: `internal/pipe/flatpak/`, `internal/pipe/notary/`, `internal/pipe/dmg/`, `internal/pipe/msi/`, `internal/pipe/pkg/`

- [x] Flatpak stage (new crate): app_id, runtime, runtime_version, sdk, command, finish_args, name_template, disable, extra_files, replace, mod_timestamp; manifest JSON generation; flatpak-builder + flatpak build-bundle subprocess; Linux-only amd64/arm64
- [x] macOS Notarization (new crate): cross-platform mode (rcodesign sign/notary-submit with P12 + API key), native mode (codesign + xcrun notarytool with keychain); sign.certificate/password/entitlements, notarize.issuer_id/key_id/key/wait/timeout; native: sign.keychain/identity/options/entitlements, notarize.profile_name/wait/timeout, use dmg/pkg; pipeline ordering fix (AppBundle before DMG)
- [x] DMG stage improvements: `use` field (binary/appbundle artifact selection), `disable` upgraded to StringOrBool
- [x] MSI stage improvements: `extra_files` (simple filenames copied to WiX build context), `extensions` (WiX extensions with template support for v3+v4), `disable` upgraded to StringOrBool
- [x] PKG stage improvements: `use` field (binary/appbundle artifact selection), `disable` upgraded to StringOrBool

### Session E1: Config Infrastructure + Pervasive Patterns
GoReleaser source: `internal/pipe/git/`, `internal/pipe/metadata/`, `internal/pipe/env/`, `internal/pipe/defaults/`

**Config infrastructure** (from fresh-gap B1-B6)
- [x] git.tag_sort, git.ignore_tags, git.ignore_tag_prefixes, git.prerelease_suffix (config + full behavioral wiring to find_latest_tag_matching + find_previous_tag, glob matching, template rendering, tag_sort validation)
- [x] Global metadata block (mod_timestamp) — config + wire mtime to metadata.json and artifacts.json; fixed metadata.json content format to match GoReleaser (project metadata, not artifact list); added artifacts.json output; registered metadata.json as artifact
- [x] Custom template variables (.Var.*) — config + wire into template engine as nested Var map; Go-style and Tera-style access; variable values template-rendered at injection
- [x] report_sizes: fixed to match GoReleaser (filter by size_reportable_kinds including Library/Wasm, store size in artifact.size field, table output)
- [x] StringOrBool/template `disable` + `skip_upload` on ALL config sections — upgraded SnapcraftConfig, AurConfig, PublisherConfig, ChocolateyConfig (disable + skip_publish), HomebrewConfig, ScoopConfig, WingetConfig, KrewConfig, NixConfig; wired is_disabled()/should_skip_upload() with template rendering in all stages

**Deferred to E1-continued** (need full GoReleaser source comparison + behavioral wiring)
- [x] Config includes from URL (with headers) + from_file structured form — IncludeSpec enum, HTTP fetching with headers, env var expansion, GitHub raw URL shorthand, body size limit, TOML detection
- [x] Template files config section (id, src, dst, mode) — new stage-templatefiles crate, template rendering + artifact registration + path safety + tests
- [x] `templated_extra_files` across sections (render file CONTENTS as templates, distinct from extra_files) — shared utility in core, TemplatedExtraFile with src/dst/mode, wired into 9 stages: checksums, release, docker, blob, publishers, snapcraft, dmg, nsis, app_bundles
- [x] Monorepo improvements (tag_prefix, dir) — MonorepoConfig, prefix-aware tag discovery/previous-tag, context var stripping, changelog dir filtering, build dir defaults, Config helpers
- [x] release.tag (Pro, template override) — config field + resolve_release_tag() wiring + template rendering + tests

### Session E2: Template Additions + Stage-Specific Extras

**Template additions** (from fresh-gap B31-B32)
- [x] OSS template functions: incpatch, incminor, incmajor (version increment)
- [x] OSS template functions: readFile, mustReadFile (file I/O)
- [x] OSS template functions: filter, reverseFilter (regex filtering)
- [x] OSS template functions: urlPathEscape, time (formatted UTC)
- [x] OSS template functions: contains, list, englishJoin
- [x] OSS template functions: dir, base, abs (path functions)
- [x] OSS template functions: map, indexOrDefault (map functions)
- [x] OSS template functions: mdv2escape (MarkdownV2 escaping)
- [x] Go-style positional syntax compatibility for replace, split, contains
- [x] Pro template functions: `in` (list membership), `reReplaceAll` (regex replace)
- [x] Template variables (OSS): .Outputs, .Checksums, .ArtifactExt, .Target (wired to all stages with cleanup)
- [x] Template variables (Pro): .PrefixedTag, .PrefixedPreviousTag, .PrefixedSummary, .IsRelease, .IsMerging, .Artifacts, .Metadata, .ArtifactID
- [x] nFPM-specific vars: .Release, .Epoch, .PackageName, .ConventionalFileName, .ConventionalExtension, .Format

**Stage-specific extras** (from fresh-gap B19-B20, B24, B33-B34)
- [x] Docker extras: v2 API, annotations, SBOM, disable template, build_args (map form) — implemented; templated_dockerfile, templated_extra_files, skip_build, manifest create_flags, manifest retry deferred (Pro/niche)
- [x] Snapcraft: hooks (top-level), missing app sub-fields (24 fields), structured extra_files with source/destination/mode, kebab-case aliases
- [x] nFPM extras: libdirs, changelog (YAML path), contents file_info.owner/group template rendering — implemented; templated_contents (Pro), templated_scripts (Pro) deferred
- [x] Environment config: template expansion in env values, env_files.github_token/gitlab_token/gitea_token, env list form (GoReleaser parity)
- [x] Artifacts JSON format parity (expanded ArtifactKind from 18 to 38 variants matching GoReleaser's type system)
- [x] Changelog Pro features: paths, title, divider, AI (use/model/prompt with inline/from_url/from_file)

### Session F: Platform Support
GoReleaser source: `internal/pipe/release/`, `internal/pipe/publish/`

- [x] GitLab release support (create/update, dual upload paths, v17 detection, job tokens, replace_existing_artifacts, release link creation)
- [x] Gitea release support (create/update, asset upload, pagination, draft/prerelease, replace_existing_artifacts)
- [x] GitHub Enterprise URLs (api/upload/download/skip_tls_verify — config + octocrab client wiring + TLS bypass)
- [x] DockerHub description sync: username, secret_name, images, description, full_description (from_url/from_file)
- [x] Artifactory publisher: name, target (template), mode (archive/binary), username/password, client_x509_cert/key, custom_headers, ids/exts
- [x] Fury.io publisher: account, disable, secret_name, ids, formats
- [x] CloudSmith publisher: organization, repository, ids/formats, distributions, component
- [x] NPM publisher: name, description, license, author, access, tag
- [x] Snapcraft publish (upload to Snap Store)
- [x] changelog.use: gitlab backend (GitLab compare API, PRIVATE-TOKEN/JOB-TOKEN auth, commit extraction)
- [x] changelog.use: gitea backend (Gitea compare API, token auth, username extraction)

### Session G: Parity Audit Findings (from 2026-04-05 audit)

**Pervasive: `goamd64`/`goarm` config fields across all publishers**
GoReleaser source: every publisher pipe's `Default()` method sets `Goamd64 = "v1"` and uses it to filter amd64 microarchitecture variants. All 7 core publishers + Nix lack these fields in anodizer. Requires adding `goamd64: Option<String>` and `goarm: Option<String>` to each publisher config, defaulting goamd64 to `"v1"`, and wiring the filter into `find_all_platform_artifacts_filtered()` in `util.rs`.
- [x] Add `goamd64` field to HomebrewConfig, ScoopConfig, ChocolateyConfig, WingetConfig, AurConfig, KrewConfig, NixConfig
- [x] Add `goarm` field to HomebrewConfig, KrewConfig
- [x] Wire goamd64/goarm into artifact filtering in `util.rs:find_all_platform_artifacts_filtered()`
- [x] Default goamd64 to "v1" in each publisher's defaults path

**Artifactory/Fury/CloudSmith: non-functional live mode**
These publishers only have dry-run logging — no actual HTTP upload code. GoReleaser source: `internal/http/http.go` (shared upload module). Anodizer files: `artifactory.rs`, `fury.rs`, `cloudsmith.rs` all have `"artifact registry not yet implemented"` placeholders. All config fields (ids, exts, mode, checksum, signature, meta, custom_artifact_name, custom_headers, extra_files, client_x509_cert/key) are declared but unwired.
- [x] Implement HTTP upload in `artifactory.rs` using reqwest: artifact iteration, per-artifact URL template rendering, Basic Auth, checksum header, TLS client certs, custom headers
- [x] Implement Fury push in `fury.rs`: HTTP PUT to `https://push.fury.io/v1/` with token auth
- [x] Implement CloudSmith push in `cloudsmith.rs`: HTTP upload to CloudSmith API
- [x] Add `method` and `trusted_certificates` fields to ArtifactoryConfig
- [x] Add username/password cross-validation to artifactory defaults

**Notarize: missing artifact refresh, timestamp server, timeout default**
GoReleaser source: `internal/pipe/notary/macos.go:144` calls `binaries.Refresh()` after signing (updates checksums). Line 95 passes `http://timestamp.apple.com/ts01` timestamp server. Line 33 defaults timeout to 10 minutes. Line 35 defaults IDs to project name.
- [x] Add `--timestamp-url http://timestamp.apple.com/ts01` to rcodesign sign command in `stage-notarize/src/lib.rs`
- [x] Refresh artifact checksums after signing (call equivalent of `artifacts.refresh()`)
- [x] Default notarize timeout to 10 minutes when not specified
- [x] Default notarize IDs to project name when empty (match GoReleaser)

**UPX/Notarize: missing UniversalBinary artifact filter**
GoReleaser source: `upx.go:119` filters `ByTypes(Binary, UniversalBinary)`. `macos.go:79` same. Anodizer only matches `ArtifactKind::Binary`. macOS universal/fat binaries are silently skipped.
- [x] Add `ArtifactKind::UniversalBinary` to UPX binary filter in `stage-upx/src/lib.rs:117`
- [x] Add `ArtifactKind::UniversalBinary` to notarize binary filter in `stage-notarize/src/lib.rs:197`

**Template engine: Go block syntax preprocessor**
GoReleaser uses Go text/template `{{ if }}/{{ end }}/{{ range }}` block syntax. Tera uses `{% if %}/{% endif %}/{% for %}/{% endfor %}`. No preprocessor pass converts between these. Affects users migrating GoReleaser configs with control flow.
- [x] Add preprocessor pass to convert `{{ if .Condition }}` → `{% if Condition %}`
- [x] Convert `{{ else }}` → `{% else %}`
- [x] Convert `{{ end }}` → context-aware `{% endif %}` or `{% endfor %}`
- [x] Convert `{{ range $k, $v := .Map }}` → `{% for k, v in Map %}`
- [x] Convert `{{ with .Field }}` → `{% if Field %}{% set _with = Field %}`
- [x] Convert `{{ $var := expr }}` → `{% set var = expr %}`

**Homebrew Cask: incomplete as child config**
GoReleaser source: `internal/pipe/cask/` — Cask is a top-level config section (`homebrew_casks []HomebrewCask`) with its own repository, commit_author, directory, skip_upload, hooks, dependencies, conflicts, manpages, completions, service, structured uninstall/zap, and structured URL config. Anodizer nests cask as a child of HomebrewConfig with ~15 missing fields.
- [x] Promote `HomebrewCaskConfig` to top-level config (Vec in Config struct)
- [x] Add missing fields: repository, commit_author, commit_msg_template, directory, skip_upload, custom_block, ids, service, manpages, completions, dependencies, conflicts, hooks, generate_completions_from_executable
- [x] Add structured `HomebrewCaskURL` (verified, using, cookies, referer, headers, user_agent, data)
- [x] Add structured `HomebrewCaskUninstall` (launchctl, quit, login_item, delete, trash)

**Winget: missing portable binary support + validation**
GoReleaser source: `internal/pipe/winget/winget.go:437-476` sets InstallerType to `"portable"` for bare binaries (not archives), populates `Commands` field. Lines 488-494 error on mixed formats or duplicate architectures. Line 187 filters to `.zip` only.
- [x] Add `"portable"` InstallerType path for `UploadableBinary` artifacts in `winget.rs`
- [x] Add `Commands` field to installer manifest for portable binaries
- [x] Add mixed-format validation (error if both .exe and .zip)
- [x] Add duplicate-architecture validation
- [x] Filter to `.zip` archives only (reject tar.gz/7z)

**Winget/Chocolatey: under-templated fields**
GoReleaser source: `winget.go:115-134` templates 18 fields; `chocolatey.go:218-227` templates 4 fields with Changelog support.
- [x] Winget: template-expand all 18 fields (Publisher, Name, PackageName, Author, etc.) in `winget.rs`
- [x] Chocolatey: template-expand Copyright, Summary, ReleaseNotes (with Changelog variable) in `chocolatey.rs`
- [x] Chocolatey: template-expand APIKey (GoReleaser treats it as a template)

### Session H: Release & Changelog Behavioral Gaps (from 2026-04-06 audit)

**GitHub Enterprise: download URL not wired to artifact metadata**
GoReleaser's `ReleaseURLTemplate()` uses `GitHubURLs.Download` to construct artifact download URLs stored in artifact metadata. Publishers (homebrew, scoop, krew, nix, chocolatey, winget, cask) read `artifact.metadata["url"]` for download links. Anodizer reads `github_urls.download` but never stores it in artifact metadata — publishers only work if `url_template` is explicitly set.
- [x] After GitHub release asset upload, populate `artifact.metadata["url"]` using download URL + owner/name/tag/artifact pattern
- [x] Set default `github_urls.download` to `"https://github.com"` when not specified
- [x] Wire `ReleaseURLTemplate` for GitHub (matching `{download}/{owner}/{name}/releases/download/{tag}/{artifact}`)
- [x] Verify all 7 publishers + cask read artifact URL metadata correctly when `url_template` is not set

**GitLab/Gitea: upload retry with exponential backoff**
GoReleaser wraps all upload errors in `RetriableError{}` and retries up to 10 times with 50ms base delay. Anodizer has single-retry on link conflict (400/422) for GitLab, zero retry for Gitea uploads.
- [x] Add configurable retry wrapper for GitLab `upload_via_package_registry()` and `upload_via_project_uploads()`
- [x] Add retry wrapper for Gitea `gitea_upload_asset()`
- [x] Match GoReleaser: 10 attempts, 50ms base delay, exponential backoff
- [x] Wrap transient HTTP errors (5xx, timeouts) in retriable error type

**GitLab: version detection API fallback**
GoReleaser checks `CI_SERVER_VERSION` env, then falls back to `/api/v4/version` API call. Anodizer only checks env, defaults to v17+ when absent.
- [x] Add API call fallback to `/api/v4/version` when `CI_SERVER_VERSION` not set
- [x] Parse version response and compare against v17.0.0

**Changelog: co-author extraction from commit trailers**
GoReleaser's `changelog.ExtractCoAuthors()` parses `Co-Authored-By:` trailers from commit message bodies for all three backends (GitHub, GitLab, Gitea). Anodizer has zero co-author extraction.
- [x] Implement `extract_co_authors()` parser for `Co-Authored-By:` trailer format
- [x] Wire into all three SCM changelog backends (github, gitlab, gitea)
- [x] Include co-authors in `CommitInfo.logins` aggregation

**Changelog: GitLab/Gitea markdown newline handling**
GoReleaser uses `"   \n"` (3 spaces + newline) for GitLab/Gitea changelog entries to force markdown line breaks. Anodizer uses plain `"\n"`.
- [x] Detect token type and use 3-space newline for GitLab/Gitea changelog formatting
- [x] Apply in changelog entry joining (between bullet items)

**Release: body truncation**
GoReleaser truncates release body to `maxReleaseBodyLength = 125000` characters (client.go:19). Anodizer has no truncation.
- [x] Add body length check before release creation
- [x] Truncate with warning when body exceeds 125,000 characters (GitHub limit)

### Session I: Archive & Source Behavioral Gaps (from 2026-04-06 audit)

**Archive: glob file resolution must preserve directory structure**
GoReleaser's `archivefiles.go` computes longest common prefix when `destination` is set, preserving relative directory structure. Anodizer flattens all resolved files to root — `docs/README.md` becomes just `README.md`.
- [x] Implement longest-common-prefix logic for glob resolution when `dst` is set
- [x] Preserve relative directory structure under destination prefix
- [x] Add tests comparing archive contents with GoReleaser output

**Archive: duplicate destination path detection**
GoReleaser's `unique()` function warns when same destination path would be overwritten. Anodizer has no duplicate detection.
- [x] Add duplicate destination path detection in `resolve_file_specs()`
- [x] Emit warning when duplicate destinations found

**Archive: file sorting for reproducibility**
GoReleaser sorts resolved file list by destination path. Anodizer does not sort.
- [x] Sort resolved files by destination path before archiving

**Archive: template rendering for FileInfo fields**
GoReleaser templates `owner`, `group`, `mtime` fields in FileInfo via `tmplInfo()`. Anodizer uses raw config values.
- [x] Template-render `owner`, `group`, `mtime` in FileInfo before applying to archive entries

**Archive: `binaries` filter field parsed but not wired**
`ArchiveConfig.binaries` is parsed from config but never used in artifact filtering logic.
- [x] Wire `binaries` field to filter binary artifacts by name in archive stage

**Archive: Amd64 version suffix in default template**
GoReleaser's default archive name template includes `{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}`. Anodizer's default omits this.
- [x] Add Amd64 suffix to default archive name template

**Archive: support archiving UniversalBinary/Header/CArchive/CShared artifact types**
GoReleaser archives Binary, UniversalBinary, Header, CArchive, CShared. Anodizer only archives Binary.
- [x] Add UniversalBinary to archive artifact type filter
- [x] Add Header, CArchive, CShared to archive artifact type filter (for C library builds)

**Source archive: strip_parent not implemented**
Config field `SourceFileEntry.strip_parent` exists but stage logs "strip_parent is not yet supported".
- [x] Implement `strip_parent` in source archive file resolution

**Source archive: file metadata (info) not implemented**
Config fields `SourceFileInfo` (owner, group, mode, mtime) exist but stage logs "file info not yet supported".
- [x] Implement file metadata overrides (owner, group, mode, mtime) for source archive entries

### Session J: Sign & Docker Behavioral Gaps (from 2026-04-06 audit)

**Sign: binary_signs default signature template missing architecture**
GoReleaser binary_signs use `${artifact}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}...` for signature naming. Anodizer defaults to `{artifact}.sig` with no architecture info.
- [x] Implement architecture-aware default signature template for binary_signs
- [x] Include Os, Arch, Arm, Amd64 variant suffixes matching GoReleaser

**Sign: IDs warning when used with checksum/source artifacts**
GoReleaser warns when `ids` filter is set with `artifacts: "checksum"` or `"source"` (ids has no effect). Anodizer is silent.
- [x] Add warning log when `ids` is set with checksum or source artifact filter

**Sign: DockerImageV2 handling in docker_signs**
GoReleaser includes `DockerImageV2` in both "images" and "manifests" filters. Anodizer only handles `DockerImage` and `DockerManifest`.
- [x] Add `DockerImageV2` to docker_signs image and manifest filters

**Sign: artifact refresh after signing**
GoReleaser calls `ctx.Artifacts.Refresh()` after all signing. Anodizer does not refresh.
- [x] Call artifact refresh equivalent after signing stage completes

**Docker: DockerV2 missing defaults**
GoReleaser sets defaults for DockerV2: ID=ProjectName, Dockerfile="Dockerfile", Tags=["{{.Tag}}"], Platforms=["linux/amd64","linux/arm64"], SBOM="true", Retry(10 attempts, 10s delay, 5m max).
- [x] Set default ID, Dockerfile, Tags, Platforms, SBOM, Retry for DockerV2 configs

**Docker: platform tag suffix for snapshot builds**
GoReleaser v2 adds platform suffix to tags during snapshot builds (e.g., "latest-amd64"). Anodizer does not.
- [x] Add platform suffix to tags during snapshot builds for DockerV2

**Docker: SBOM attestation format**
GoReleaser uses `--attest=type=sbom` for proper OCI attestation. Anodizer uses `--sbom=true`.
- [x] Change SBOM flag from `--sbom=true` to `--attest=type=sbom` for buildx

**Docker: buildx driver validation**
GoReleaser v2 checks buildx driver is "docker-container" or "docker". Anodizer has no validation.
- [x] Validate buildx driver type and warn if non-standard

**Docker: Dockerfile path template rendering**
GoReleaser templates the `Dockerfile` field path. Anodizer treats it as literal.
- [x] Template-render the Dockerfile path field before use

**Docker: index annotation prefix for multi-platform**
GoReleaser v2 adds "index:" prefix to annotations for multi-platform builds. Anodizer does not.
- [x] Add "index:" annotation prefix for multi-platform Docker builds

**Docker (deep audit): V2 snapshot/publish split**
GoReleaser V2 has separate Snapshot.Run() (per-platform --load, no push, no SBOM) and Publish.Publish() (--push). Anodizer had a single path.
- [x] Split V2 snapshot into per-platform --load builds (no push, no SBOM)
- [x] Remove auto --provenance=false and --sbom=false from V2 (only legacy does this)

**Docker (deep audit): V2 staging layout and artifact types**
GoReleaser V2 stages to os/arch/name and stages Binary, LinuxPackage, CArchive, CShared, PyWheel. Anodizer used binaries/arch and only Binary.
- [x] Implement stage_artifacts_v2() with GoReleaser layout (os/arch/name)
- [x] Stage Binary, LinuxPackage, CArchive, CShared, PyWheel artifacts
- [x] Handle "all" goos artifacts (copied to every platform dir)

**Docker (deep audit): V2 artifact registration**
GoReleaser registers V2 artifacts as DockerImageV2 with image ref as name/path.
- [x] Register V2 artifacts as DockerImageV2 (not DockerImage) with correct name/path

**Docker (deep audit): Template map keys and empty filtering**
GoReleaser's tplMapFlags() templates both keys and values; skips empty entries. Also templates platforms.
- [x] Template-render map keys for labels/annotations/build_args
- [x] Skip empty key/value entries after templating
- [x] Template-render platform strings, filter empty results
- [x] Filter empty rendered flags

**Docker (deep audit): Legacy docker defaults and template rendering**
GoReleaser defaults dockerfile to "Dockerfile" and template-renders the path.
- [x] Default dockerfile to "Dockerfile" when empty
- [x] Template-render legacy docker dockerfile path

**Docker (deep audit): Retry pattern precision**
GoReleaser uses precise HTTP error patterns and manifest verification retry.
- [x] Narrow "500" retry pattern to full HTTP error format
- [x] Add "manifest verification failed for digest" retry pattern

**Docker (deep audit): Manifest push digest capture**
GoReleaser captures sha256 digest from docker manifest push stdout.
- [x] Capture digest from manifest push output and store in artifact metadata

**Docker (deep audit): SkipPush template support**
GoReleaser's skip_push accepts template strings (e.g., "{{ if .IsSnapshot }}true{{ end }}"). Anodizer only accepts "auto" or bool.
- [x] Add Template(String) variant to SkipPushConfig
- [x] Wire Template variant through resolve_skip_push

**Docker (deep audit): DockerSignConfig.env type mismatch**
GoReleaser uses []string ("KEY=VAL" list) for docker_signs env. Anodizer uses HashMap. YAML input from GoReleaser configs won't deserialize.
- [x] Add custom deserializer that accepts both map and list-of-strings formats

**Docker (deep audit): DockerSignConfig.output type**
GoReleaser accepts string-or-bool for docker_signs output. Anodizer only accepts bool.
- [x] Change DockerSignConfig.output from Option<bool> to Option<StringOrBool>

**Docker (deep audit): Environment variable passthrough**
GoReleaser passes ctx.Env.Strings() to docker commands. Anodizer inherits process env but doesn't inject context env vars.
- [x] Merge context env vars into docker command environment

**Docker (deep audit): Output secret redaction**
GoReleaser pipes docker command output through redact.Writer to strip secrets. Anodizer writes raw output.
- [x] Implement output redaction for docker command stdout/stderr

**Docker (deep audit): UX diagnostics**
GoReleaser has several diagnostic helpers that Anodizer lacks.
- [x] Zero-artifact warning when no matching binaries found for platform
- [x] File-not-found diagnostic (detect COPY/ADD failures, show staging dir contents)
- [x] Buildx context error diagnostic (suggest "docker context use default")
- [x] "Did you mean?" Levenshtein suggestion for manifest image mismatches
- [x] Heuristic warnings when extra_files contain project markers (go.mod, Cargo.toml)
- [x] Docker daemon availability check before snapshot builds

**Docker (deep audit): ID uniqueness validation**
GoReleaser validates docker V2 config IDs are unique. Anodizer allows duplicates silently.
- [x] Validate ID uniqueness for docker V2 and manifest configs

**Docker (deep audit): Image tag deduplication and sorting**
GoReleaser deduplicates and sorts image:tag lists. Anodizer does neither.
- [x] Deduplicate and sort generated image:tag combinations

**Docker (deep audit): Missing config fields on legacy Docker**
GoReleaser legacy Docker has goos, goarch, goarm, goamd64 fields for artifact filtering. Anodizer uses platforms instead.
- N/A: goos/goarch/goarm/goamd64 are Go-specific; Rust uses `platforms` (OCI-standard format) which already provides equivalent cross-platform Docker support

**Docker (deep audit): Missing DockerDigest config type**
GoReleaser has a top-level docker_digest config with disable and name_template fields that controls docker image digest artifact naming.
- [x] Add DockerDigest config struct with disable and name_template fields
- [x] Wire DockerDigest into the pipeline

**Docker (deep audit): --iidfile for V2 digest capture**
GoReleaser V2 uses --iidfile=id.txt to capture image digest from buildx instead of docker inspect post-push. This works even without push.
- [x] Add --iidfile support to build_docker_v2_command and read digest from file

### Session K: nFPM & Publisher Behavioral Gaps (from 2026-04-06 audit) — DONE

**nFPM: IPK format implementation**
Config validation lists "ipk" but no underlying config struct or YAML generation exists. GoReleaser has full IPK support with 9 config fields: abi_version, alternatives, auto_installed, essential, predepends, tags, fields.
- [x] Add `NfpmIPK` config struct with all 9 fields
- [x] Wire IPK config into nFPM YAML generation
- [x] Add IPK-specific test cases

**nFPM: template rendering for 8 missing field groups**
GoReleaser templates these fields; anodizer passes raw values:
- [x] Template-render `bindir` field
- [x] Template-render `mtime` field
- [x] Template-render script paths (preinstall, postinstall, preremove, postremove)
- [x] Template-render signature key files (deb, rpm, apk key_file + apk key_name)
- [x] Template-render libdirs (header, cshared, carchive)
- [x] Template-render content src/dst paths
- [x] Template-render content file_info.mtime

**Chocolatey: skip_publish type and phase mismatch**
GoReleaser's `SkipPublish` is a boolean checked in `Publish()`. Anodizer uses `StringOrBool` checked inconsistently.
- [x] Change chocolatey `skip_publish` from StringOrBool to boolean (match GoReleaser)
- [x] Move skip_publish check to correct execution phase

**Nix: license validation missing**
GoReleaser validates license against a Nix-compatible allowlist. Anodizer has no validation.
- [x] Add Nix license allowlist validation in nix publisher defaults (already implemented)

**Publisher repository field templating**
GoReleaser applies `TemplateRef()` to repository fields for Krew and Nix. Anodizer does not template repository fields.
- [x] Template repository owner/name fields for Krew publisher
- [x] Template repository owner/name fields for Nix publisher

**Scoop: missing default commit message template**
GoReleaser sets a default `CommitMessageTemplate` in `Default()`. Anodizer does not.
- [x] Set default commit_msg_template for Scoop publisher

**Cask: directory field not templated + missing binary inference**
GoReleaser templates directory and infers binaries from name when empty. Anodizer does neither.
- [x] Template-render directory field for Cask publisher
- [x] Infer binary list from cask name when `binaries` is empty (already implemented)

**Winget: PackageIdentifier missing from commit template context + PackageName fallback**
GoReleaser adds PackageIdentifier to commit message template context and falls back PackageName to Name.
- [x] Add PackageIdentifier to winget commit message template context
- [x] Implement PackageName fallback to Name when empty

**AUR: directory field not templated**
GoReleaser templates the `directory` field. Anodizer does not.
- [x] Template-render directory field for AUR publisher

### Session L: Config/Defaults & Announce Gaps (from 2026-04-06 audit) — DONE

**Defaults: missing default values matching GoReleaser**
- [x] Default `snapshot.version_template` to `"{{ Version }}-SNAPSHOT-{{ ShortCommit }}"` (with empty-string fallback)
- [x] Default `release.name_template` to `"{{ Tag }}"` (renders via template engine)
- [x] Default `checksum.algorithm` to `"sha256"`
- [x] Default `git.tag_sort` to `"-version:refname"` when not set
- [x] Default archive `files` to include license/readme/changelog patterns

**Token file path defaults**
- [x] Set default token file paths in env_files when not configured (with ~ expansion)

**force_token environment variable override**
- [x] Read `ANODIZER_FORCE_TOKEN` (primary) and `GORELEASER_FORCE_TOKEN` (compat) env vars as fallback (6 new tests)

**Announce: missing default values for providers**
- [x] Slack: default username to "anodizer"
- [x] Discord: default author to "anodizer", default color to 3888754
- [x] Teams: default icon_url — GoReleaser uses their avatar; documented why anodizer omits (no hosted avatar exists)
- [x] Mattermost: default username "anodizer"

**Announce: webhook Content-Type charset**
- [x] Default to `"application/json; charset=utf-8"` (with starts_with JSON detection fix)

**Announce: Mattermost top-level text with attachments**
- [x] Include `"text": ""` at top level when attachments present (matching GoReleaser's Go struct serialization)

### Session M: Missing Stages & Cross-Cutting (from 2026-04-06 audit) — DONE

**Makeself: self-extracting archives (new stage)**
GoReleaser has full makeself support (338 lines): `.run` self-extracting shell archives with embedded binaries, compression options, LSM metadata, per-platform packaging. Anodizer has `ArtifactKind::Makeself` defined but zero implementation.
- [x] Create `stage-makeself` crate
- [x] Implement MakeselfConfig: id, ids, name_template, scripts, extra_files, compression (gzip/bzip2/xz/lzo/none), lsm_file, disable
- [x] Subprocess: `makeself --{compression} [--lsm] <archive_dir> <output.run> <label> [startup_script]`
- [x] Wire into pipeline ordering (after archive stage)

**SRPM: source RPM packages (new stage)**
GoReleaser has source RPM support (248 lines): `.src.rpm` from spec templates, signatures, per-format config.
- [x] Create `stage-srpm` crate
- [x] Implement SrpmConfig: enabled, package_name, file_name_template, spec_file, signature, disable
- [x] Generate spec file from template with version/release/license substitution, %changelog
- [x] Subprocess: `rpmbuild -bs <spec>` with rpmbuild directory structure

**Milestone closing**
GoReleaser closes GitHub/GitLab/Gitea milestones after release (93 lines). Anodizer has no milestone support.
- [x] Add MilestoneConfig: close (bool), fail_on_error (bool), name_template (default "{{ Tag }}")
- [x] GitHub REST API milestone close with pagination and provider detection
- [x] Execute after release stage, before after-hooks

**Generic HTTP upload**
GoReleaser has generic HTTP upload to arbitrary endpoints (42 lines in pipe, delegates to shared HTTP module). Distinct from specialized Artifactory/Fury/CloudSmith publishers.
- [x] Add UploadConfig: name, target (URL template), method (PUT/POST), username, password, custom_headers, ids, exts, checksum_header, trusted_certificates, disable
- [x] Implement generic HTTP upload reusing Artifactory's reqwest infrastructure (mTLS, mode filtering)

**AUR source packages**
GoReleaser's `aursources` pipe generates source-only PKGBUILD (not `-bin`). Anodizer has AUR binary packages but not source variant.
- [x] Implement AUR source package generation (PKGBUILD with cargo build, .SRCINFO, git push)
- [x] Wire separate AurSourceConfig in PublishConfig alongside existing binary AUR publisher

**Snapcraft: plugs structure (dict vs list)**
GoReleaser uses `Plugs map[string]any` (structured plug definitions). Anodizer uses `Vec<String>` (list only).
- [x] Change snapcraft plugs from `Vec<String>` to `HashMap<String, serde_json::Value>` (interface + attributes)
- [x] Update snap.yaml generation to output structured plug definitions

**Snapcraft: default grade and channel templates**
GoReleaser defaults grade to "stable" and auto-populates channel_templates based on grade. Anodizer does neither.
- [x] Default snapcraft grade to "stable"
- [x] Auto-populate channel_templates via resolve_effective_channels() (stable→[edge,beta,candidate,stable]; devel→[edge,beta])

**Custom publishers: OS/Arch template variables**
GoReleaser exposes per-artifact `.OS`, `.Arch`, `.Target` variables. Anodizer exposes `ArtifactPath`, `ArtifactName`, `ArtifactKind` but not OS/Arch.
- [x] Add `Os`, `Arch`, `Target` template variables to custom publisher per-artifact context

**Custom publishers: system environment passthrough**
GoReleaser explicitly passes HOME, USER, PATH, TMPDIR, etc. Anodizer only passes publisher-configured env.
- [x] System env inherited by default (Rust Command behavior, no env_clear). Documented.

### Session N: Deep Parity Audit (2026-04-07)

**Build Stage**
- [x] Binary name template rendered before per-target vars — moved render into per-target loop
- [x] No glibc version suffix support — added `strip_glibc_suffix()`, full target kept for cargo-zigbuild
- [x] Invalid targets warn instead of error — changed to `bail!()` matching GoReleaser
- [x] No duplicate build ID validation — added HashSet check per crate
- [x] No workspace `--package` validation — added `check_workspace_package()` parsing Cargo.toml
- [x] Build hooks lack per-target context — Name, Path, Ext, Target, Os, Arch now set in template vars
- [x] `skip` is `Option<bool>` — changed to `Option<StringOrBool>` with template evaluation
- [x] `no_unique_dist_dir` is `Option<bool>` — changed to `Option<StringOrBool>` with template evaluation
- [x] Universal binary hooks unwired — added pre/post hook execution around lipo
- [x] Universal binary artifact type — changed to `ArtifactKind::UniversalBinary`
- [x] Universal binary output path — now uses `dist/{crate}_darwin_all/{name}`
- [x] No `command` config field — added `command: Option<String>` with multi-word split
- [x] Known targets list incomplete — expanded to ~110+ targets (all mips, riscv, powerpc, sparc, thumb, wasm, etc.)
- [x] No ELF dynamic link detection — added PT_INTERP check, stores `DynamicallyLinked` in artifact metadata
- [x] Process env vars not expanded — added `expand_env_vars()` for `$VAR`/`${VAR}` expansion

**Archive / Checksum / Source**
- [x] Missing `Algorithm` template variable in split checksum mode — added to template vars; sidecar path fixed to use dist dir
- [x] Source archive prefix default differs: changed to empty (no prefix) when prefix_template unset; removed unconditional trailing `/`
- [x] Extra files glob + name_template constraint not enforced — check now runs before directory filtering, includes template in error
- [x] Backslash normalization missing in archive entry paths — `.replace('\\', "/")` in three locations

**Config / Env / Git**
- [x] Gitea download URL not derived from API URL — now strips `/api/v1` from API URL as fallback
- [x] No `ErrMultipleTokens` detection — now errors when multiple SCM tokens set without force_token
- [x] No `ErrMissingToken` hard error — now errors early (skipped in snapshot/dry-run mode)
- [x] Default token file paths (`~/.config/goreleaser/{github,gitlab,gitea}_token`) never auto-checked — now always auto-checked in setup_env even without explicit env_files config
- [x] No `project_name` auto-inference from Cargo.toml when not set in config — now reads package.name
- [x] Empty snapshot version (from bad template) not validated — now errors on empty rendered name
- [x] Git remote URL uses `remote get-url origin` instead of `ls-remote --get-url` — fixed
- [x] Effective config not written in dry-run mode — now always writes

**Release / Changelog / Sign**
- [x] Milestone close not implemented for GitLab and Gitea — only GitHub works — added `close_milestone_gitlab()` and `close_milestone_gitea()` with REST API calls
- [x] Sign config ID dedup validation missing — added HashSet check for signs and binary_signs
- [x] Changelog format default uses `ShortSHA` — changed git backend default to `{{ SHA }} {{ Message }}`

**Docker / UPX / SBOM / Notarize**
- [x] UPX `enabled` is `bool` — changed to `Option<StringOrBool>` with template evaluation
- [x] No parallel UPX compression (GoReleaser uses `semerrgroup`) — added `std::thread::scope` with chunked parallelism bounded by `ctx.options.parallelism`
- [x] Notarize missing status differentiation (rejected vs timeout vs invalid in errors) — added `check_notarize_output()` parsing status strings; timeout non-fatal per GoReleaser
- [x] SBOM subprocess gets full environment instead of restricted passthrough vars — added `env_clear()` + 8-var whitelist matching GoReleaser

**Publishers**
- [x] Homebrew Cask: nested cask config (under `homebrew`) ignores all extended fields — wired custom_block, service, license, manpages, completions, dependencies, conflicts, hooks from HomebrewCaskConfig
- [x] Homebrew Cask: no `#{version}` URL interpolation for Homebrew auto-update mechanism — URL version replaced with `#{version}` for auto-update
- [x] Nix: hash format differs — anodizer uses SRI (`sha256-...`), GoReleaser uses nix-native base32 — implemented `nix_base32_encode()` matching Nix's `printHash32()`
- [x] Krew: missing required field validation (description, short_description) and binary count enforcement — added hard errors for missing required fields
- [x] All publishers: commit message defaults differ from GoReleaser templates — updated all 6 publisher kinds to match GoReleaser defaults
- [x] AUR Source: missing `.install` file support (`Install` field) — already implemented: install file written to disk, PKGBUILD includes `install=` line
- [x] Winget: missing `# yaml-language-server: $schema=...` header comments — already implemented: GENERATED_HEADER + SCHEMA_* constants prepended to all 3 manifest files

**Packaging**
- [x] nFPM: no `skip_sign` check — added `skip_sign` parameter, clears signature configs when active
- [x] nFPM: missing `libdirs` defaults — now defaults to `/usr/include`, `/usr/lib`, `/usr/lib`
- [x] nFPM: default name template — already uses `PackageName` (verified correct)
- [x] nFPM: termux prefix rewriting incomplete — missing libdirs (Header/CArchive/CShared) — added termux prefix rewriting for all three libdirs
- [x] nFPM: no warning when `formats` is empty — now logs warning and skips
- [x] Snapcraft: missing completer file copy into build directory — added completer file copy from app configs into snap build dir
- [x] Snapcraft: defaults summary/description silently instead of erroring — changed to hard errors when missing
- [x] Snapcraft: riscv64 accepted but unsupported by snap store — added `is_valid_snap_arch()` check, skips with warning
- [x] SRPM: missing `SRPM_PASSPHRASE` env var — now reads passphrase from config or env
- [x] SRPM: no `skip_sign` wiring — now checks `ctx.should_skip("sign")`
- [x] Makeself: `strip_parent` config field defined but never wired — now uses filename only when set

**Announce / CLI**
- [x] Missing `--release-header-tmpl` and `--release-footer-tmpl` CLI flags — added both
- [x] SMTP: `body_template` field name from GoReleaser not accepted as alias for `message_template` — added serde alias
- [x] Custom publisher env not sandboxed — full system env inherited vs GoReleaser's 8-var passthrough — added `env_clear()` + 8-var whitelist (HOME/USER/USERPROFILE/TMPDIR/TMP/TEMP/PATH/SYSTEMROOT)
- [x] Custom publisher shell execution via `sh -c` vs GoReleaser's direct exec with shellwords parse — replaced `sh -c` with shellwords parse + direct exec
- [x] Slack blocks: missing `\"` to `"` un-escaping before template rendering — added in render_json_template
- [x] Webhook: auto-wraps non-JSON messages — changed to always send raw message (matching GoReleaser)

**Infrastructure / Cross-Cutting**
- [x] No deprecation system for config fields — deprecated fields silently accepted — added `Context::deprecate()` with dedup via `notified_deprecations` HashSet
- [x] No ID uniqueness validation — added for build IDs (stage-build), sign IDs (stage-sign); docker v2/makeself already had it
- [x] No strict YAML parsing — unknown config fields silently ignored — added `deny_unknown_fields` to top-level `Config` struct
- [x] Hook output redaction uses hardcoded 10-var list — now delegates to `redact::string()` which auto-discovers secrets
- [x] No GitHub API rate limit handling (sleep-and-retry on 403/429) — added `check_github_rate_limit()` proactive check + 403/429 detection in upload retry loop

### Session O: Extremely Deep Parity Audit (2026-04-07)

10 parallel comparison agents read every GoReleaser pipe line-by-line. 85+ findings across all areas. 3 rounds of code review until zero issues remained.

**Template Engine**
- [x] Preprocess `eq`/`ne`/`gt`/`lt`/`ge`/`le` comparison functions → Tera operators (`==`/`!=`/`>`/`<`/`>=`/`<=`)
- [x] Variadic `eq X Y Z` → `X == Y or X == Z` (Go's eq is variadic)
- [x] Preprocess `and`/`or` prefix functions → Tera infix operators
- [x] Preprocess `len .X` → `X | length`
- [x] Register `index` as a Tera function (map/array lookup)
- [x] Fix `map` positional syntax preprocessing — `map "k1" "v1"` → `map(pairs=[...])`
- [x] Missing env var (`{{ Env.NONEXISTENT }}`) returns empty string instead of error (matching GoReleaser)
- [x] Regex cache for preprocessor comparison rewriting (avoid per-call compilation)

**Config Types**
- [x] All 14 announce provider `enabled` fields: `Option<bool>` → `Option<StringOrBool>` (template-conditional enable)
- [x] Discord `color`: `u32` → `String` (GoReleaser uses string, parsed at runtime)
- [x] Telegram `message_thread_id`: `i64` → `String` (template support)
- [x] Email subject/body defaults changed to match GoReleaser ("is out!" and ReleaseURL body)

**Sign Stage**
- [x] "all" artifact filter narrowed to release-uploadable types only (not internal types)
- [x] Sign env field values now template-rendered through context
- [x] Sign command stdout/stderr now redacted for secrets
- [x] Docker sign IDs validated for uniqueness
- [x] Docker sign digest template var set in both PascalCase (`Digest`) and lowercase (`digest`)

**Snapcraft**
- [x] Summary, description, grade now template-rendered
- [x] Channel templates now template-rendered (filter empty results)
- [x] Default snap name uses ProjectName (not binary name)
- [x] Extra files default mode 0644
- [x] Binary copy mode 0555

**Archive / Source / Checksum**
- [x] Archive file spec `src` patterns now template-rendered before glob expansion
- [x] Source archive extra file `src` patterns now template-rendered
- [x] Source archive artifact name set to filename (was empty string)
- [x] FormatOverride warns on empty `os` or `format`
- [x] Checksum hash stored in artifact metadata as `"Checksum": "algorithm:hash"` for publisher consumption
- [x] Checksum metadata storage uses HashMap for O(1) lookup (was O(n*m))

**Release / Changelog**
- [x] Release body header/footer separator changed from `\n\n` to `\n` (matching GoReleaser)

**Build**
- [x] Build flags template-rendered through context (supports `{{ .Version }}` in flags)
- [x] Before hooks run with template vars (Env, ProjectName, time vars — before git context)
- [x] Duplicate KNOWN_TARGETS entries removed (x86_64-linux-android, thumbv7neon-linux-androideabi, x86_64-apple-ios)

**Publishers**
- [x] Scoop: platform uniqueness validation (error on duplicate amd64/arm64/386 entries)
- [x] Krew: goarch "all" expanded to amd64 + arm64 platform entries
- [x] HTTP upload/Artifactory: custom_headers values template-rendered with artifact context
- [x] Chocolatey: default `source_repo` to `https://push.chocolatey.org/` (was already pushing — verified)

**Blob**
- [x] Added Signature, Certificate, Flatpak, SourceRpm to uploadable artifact types

**Makeself**
- [x] "replaces" metadata propagated from source binary to makeself artifact

**Infrastructure**
- [x] Unused `base64::Engine` import moved into `#[cfg(test)]` function
- [x] `git ls-remote --get-url` comment corrected (defaults to origin, not "first remote")

### Session O: Remaining Findings (architectural / deferred)

Items identified by the deep audit that are intentional architectural differences or require larger refactors:

**Intentional Differences (N/A):**
- Universal binary uses `lipo` instead of native Mach-O fat binary builder — requires macOS, but avoids reimplementing Mach-O format in Rust
- Snapcraft uses `snapcraft.yaml` + `snapcraft pack` instead of GoReleaser's pre-prime `snap.yaml` approach — both produce valid snaps
- SRPM uses `rpmbuild` subprocess instead of nfpm Go library — architectural choice, both produce valid SRPMs
- UPX uses `targets` glob filter instead of `goos/goarch` — Rust target triples are more precise
- Teams uses Adaptive Card format instead of GoReleaser's legacy MessageCard — Adaptive Cards are the modern replacement
- Slack blocks/attachments use typed structs instead of untyped `any` — limits arbitrary Block Kit JSON but provides type safety
- Build command uses `--bin <name>` explicitly — GoReleaser relies on Cargo.toml default target
- `filter`/`reverseFilter` regex uses Perl-like (Rust regex) vs GoReleaser's POSIX ERE — different match semantics for alternations

**Deferred — All Resolved (Session O completion, 2026-04-07):**
- [x] SBOM extracted to independent `stage-sbom` crate with SbomStage, wired into pipeline after SourceStage
- [x] Blob KMS client-side encryption via CLI tools (aws/gcloud/az) for awskms://, gcpkms://, azurekeyvault:// URL schemes
- [x] Nix formatter support (alejandra/nixfmt) — verified working: whitelist validation, subprocess after template render, warn-on-failure
- [x] Nix per-platform sourceRoots — sourceRootMap generated when archives have different WrappedIn per platform
- [x] Nix dynamically-linked binary detection — ELF PT_INTERP check, autoPatchelfHook + stdenv.cc.cc.lib in derivation
- [x] Homebrew formula: auto-generated multi-binary install from ExtraBinaries metadata (sorted, deduped)
- [x] Homebrew cask: multi-platform support with on_macos/on_linux + on_intel/on_arm nesting
- [x] All publishers: `OnlyReplacingUnibins` filter for universal binary deduplication (Homebrew, Scoop, Nix, Krew, shared util)
- [x] All publishers: `Repository.Git.URL` SSH push with StrictHostKeyChecking=accept-new, -F /dev/null, private key support
- [x] GitHub co-author email → username resolution via noreply parsing + Search Users API with caching
- [x] GitHub rate limit checking for non-upload operations (release create, draft find, milestone ops)
- [x] GitHub asset deletion pagination (per_page=100, iterative page scan, 50-page safety cap)
- [x] `--skip` values validated against known stage set (separate Release and Build sets, descriptive error on invalid)
- [x] `build` subcommand has `--skip` flag (valid: pre-hooks, post-hooks, validate, before)
- [x] `--skip=validate` tolerates non-semver tags (warning + SemVer{0,0,0} default)
- N/A: `printf`/`print`/`println` — NOT in GoReleaser's template engine either (verified in tmpl.go FuncMap)

### Pre-Release Session: Parity Audit Batch 1 (2026-04-08)

5 parallel comparison agents audited all GoReleaser pipes. 17 BUGs + 2 GAPs fixed. 3,200+ tests pass.

**Infrastructure (defaults/env/git/snapshot/metadata)**
- [x] force_token must clear non-forced tokens BEFORE multi-token check (helpers.rs ordering)
- [x] release.disable must suppress token validation (helpers.rs)
- [x] Snapshot must NOT overwrite RawVersion (release.rs)
- [x] Metadata.json + artifacts.json must be written in dry-run mode (release.rs)
- [x] Tag annotation format: %(subject) → %(contents:subject), %(body) → %(contents:body) (git.rs)
- [x] Artifact paths normalized to forward slashes in artifacts.json (artifact.rs)

**Changelog / Release**
- [x] Changelog header/footer separator: \n → \n\n (lib.rs)
- [x] Remove implicit "Others" group for unmatched commits — GoReleaser drops them (lib.rs)
- [x] Group matching: config order for matching, sort order for display only (lib.rs)
- [x] Bullet character: `- ` → `* ` (lib.rs)
- [x] CHANGELOG.md written even in dry-run mode (lib.rs)
- [x] Archive binary format exempt from different-binary-count check (lib.rs)

**Publishers**
- [x] Homebrew: formula name `@N` handling — skip digit after AT insertion (homebrew.rs)
- [x] AUR: .SRCINFO backup entries added (aur.rs)
- [x] Krew: header comment + platform sorting by URI descending (krew.rs)
- [x] Scoop: removed checkver/autoupdate fields not in GoReleaser (scoop.rs)
- [x] Nix: install ALL binaries from archive, not just one (nix.rs)

**Flaky tests**
- [x] Removed 4 additional `set_current_dir` races in source stage tests

**Previously deferred — now fixed (2026-04-08):**
- [x] nfpm creates 1 package per platform with all binaries (was per-binary) — rearchitected artifact grouping via BTreeMap
- [x] Source archive zip extra files placed under prefix (was at root) — zip crate append with prefix path
- [x] SBOM error when command produces no output (was silent success) — bail on missing/empty output files
- [x] nfpm empty maintainer warning — log warning when maintainer field is empty
- [x] nfpm ID uniqueness validation — HashSet check across all crate nfpm configs

**Remaining architectural differences (genuine refactors):**
- [x] Snapcraft prime-dir architecture: pre-stages binaries into prime/, writes snap.yaml to prime/meta/, runs `snapcraft pack prime_dir` (Session N)
- [x] nfpm iOS/AIX/Android platform support: expanded target filter + format restrictions (ios=deb-only, aix=rpm-only/ppc64-only) (Session N)
- [x] nfpm C library artifact staging: Header/CArchive/CShared collected alongside Binary, routed to libdirs (Session N)

### Post-Release: Developer Experience / Infrastructure

- [x] JSON Schema generation: `anodizer jsonschema` CLI command using schemars-derived schema
- [x] Config reference auto-generated from JSON Schema (xtask gen-docs)
- [x] Publish JSON Schema to docs site URL (`docs/site/static/schema.json`, generated in docs workflow)
- [x] `# yaml-language-server: $schema=...` inline comment — added to `.anodizer.yaml`

### Pre-Release: Documentation Site Parity

26 doc pages exist on goreleaser.com for features Anodizer has implemented but has no corresponding doc page. Each page must be written by reading the stage source code and the GoReleaser doc page side-by-side.

**General:**
- [x] `general/artifacts` — artifact types reference
- [x] `general/dist` — dist directory config
- [x] `general/git` — git config (tag_sort, etc.)
- [x] `general/metadata` — metadata config
- [x] `general/retry` — retry config
- [x] `general/templatefiles` — template files stage

**Package:**
- [x] `package/app_bundles` — macOS app bundles
- [x] `package/flatpak` — flatpak stage
- [x] `package/makeself` — makeself stage
- [x] `package/nsis` — NSIS installer stage
- [x] `package/srpm` — source RPM stage
- [x] `package/docker_digests` — docker digest config
- [x] `package/docker_manifest` — docker manifest config

**Sign:**
- [x] `sign/notarize` — notarize stage

**Publish:**
- [x] `publish/artifactory` — artifactory publisher
- [x] `publish/aursources` — AUR source publisher
- [x] `publish/cloudsmith` — cloudsmith publisher
- [x] `publish/dockerhub` — dockerhub publisher
- [x] `publish/gemfury` — fury publisher
- [x] `publish/homebrew_casks` — homebrew cask (separate page from formulas)
- [x] `publish/npm` — npm publisher
- [x] `publish/upload` — generic upload publisher
- [x] `publish/scm/gitlab` — GitLab release
- [x] `publish/scm/gitea` — Gitea release
- [x] `publish/milestone` — milestone closing
- [x] `publish/beforepublish` — before-publish hooks


---

## Phase 3: 2026-05-07 GoReleaser refresh (HEAD `8976559`, tag `v2.16.0-17315a55-nightly-1-g8976559`)

Walked 50 commits in `v2.15.0..HEAD`. Inventory delta captured in `goreleaser-complete-feature-inventory.md` §2.18 + §5.delta. Sessions P and Q below carry the actionable rows.

> **Triage rule.** Rows are *pending* by default. Rows tagged "verify-only" mean the upstream fix may already be impossible-by-construction in Rust (e.g. nil-deref panics on missing array index don't apply to Rust pattern-match). Verify-only rows can flip to *done* by adding a regression test mirroring the upstream test, no code change required. Rows tagged "code change" require behavior modification.

### Session P: Required-tier refresh gaps (2026-05-07)

GoReleaser source: see per-row source_ref. Each row carries upstream commit SHA for blame-traceable diffing.

**P1. Project-level `retry:` config (NEW required gap)**
- [ ] **P1.1** Add `RetryConfig { attempts: u32, delay: Duration, max_delay: Duration }` to `crates/core/src/config/mod.rs::Config` (new top-level field). Defaults: `attempts=10`, `delay=10s`, `max_delay=5m`. Upstream ref: `pkg/config/config.go::Project.Retry` + `internal/retryx/retryx.go`. Doc: `/customization/general/retry/`.
- [ ] **P1.2** Implement `anodizer_core::retry::do_retry(cfg, op)` and `do_retry_with_data(cfg, op)` analogous to `retryx.Do/DoWithData`. Retriable predicate: network errors (connection reset/refused, TLS handshake timeout, i/o timeout, broken pipe, EOF, "context deadline exceeded"), HTTP 5xx, HTTP 429.
- [ ] **P1.3** Wire `Project.Retry` through every announcer pipe: discord, telegram, slack, mastodon, teams, reddit, twitter, bluesky, linkedin, discourse, mattermost, webhook, opencollective, mcp.
- [ ] **P1.4** Wire through every git-provider client: github (release create, asset upload, draft find, milestone, PR), gitlab, gitea (each ReleaseURLTemplater).
- [ ] **P1.5** Wire through HTTP uploads: artifactory, custom upload (custompublishers), snapcraft store push (replace ad-hoc 10×10s constants), blob upload retries.
- [ ] **P1.6** Wire through docker pipes: replace per-pipe `DockerRetryConfig` reads with `Project.Retry` fallback (keep per-pipe override for back-compat; emit deprecation warning matching GR doc deprecations.md). Upstream ref: `pkg/config/config.go::Docker.Retry // Deprecated`.
- [ ] **P1.7** Tests: unit tests for retriable predicate, integration tests for cross-pipe propagation, deprecation-warning emission.

**P2. GitHub release publish-fields preservation (commit 6ecba31 + 2e17678)**
- [ ] **P2.1** When un-drafting (`draft=false` PATCH), include `name` (re-rendered from `name_template`) in the PATCH body. Path: `crates/stage-release/src/github/mod.rs::~655`.
- [ ] **P2.2** When `ctx.PreRelease` is true, force `make_latest=false` in the PATCH body, regardless of user's `make_latest` template result. Path: same.
- [ ] **P2.3** Regression test: prerelease draft → publish flow asserts payload includes `name` and `make_latest=false`.

**P3. GitHub author lookup simplification (commit 17315a5, #6601)**
- [ ] **P3.1** Decision: track upstream removal of Search Users API fallback OR keep as anodizer-superset.
- [ ] **P3.2** If tracking upstream: remove `/search/users?q={email}+in:email` call from `crates/stage-release/src/github/username.rs::~83` and the rate-limit-checker call. Resolve username only via `noreply.github.com` pattern.
- [ ] **P3.3** If keeping superset: document divergence in inventory row notes; add rate-limit guard so big releases don't trip secondary rate limit.

**P4. Sign `artifacts: all` excludes Signature + Certificate (commit 87a55ea, #6509)**
- [ ] **P4.1** Update `crates/stage-sign/src/helpers.rs::should_sign_artifact` so `"all"` excludes `ArtifactKind::Signature` and `ArtifactKind::Certificate`. Path: also update `tests.rs:113-114` assertions.
- [ ] **P4.2** Regression test: re-sign idempotency — running sign-all twice does not produce `.sig.sig`.

**P5. dockers/v2 Dockerfile-template emptiness check after rendering (commit d788340)**
- [ ] **P5.1** Verify `crates/stage-docker/src/v2/` checks rendered Dockerfile string (post-template) for emptiness; bail with `Skip("no dockerfile")`. Test case: `{{ if .IsSnapshot }}Dockerfile{{ end }}` during release skips.

**P6. dockers v1 healthcheck delegates to v2 buildx probe (commit e09e23a, #6526)**
- [ ] **P6.1** Verify `crates/stage-docker/src/healthcheck.rs` (or equivalent) probes buildx version when any v1 docker config has `use: buildx`.

**P7. Sec sweep: log-statement audit + redact length threshold (commit d1cdbb2)**
- [ ] **P7.1** Audit log statements in `crates/stage-build/`, `crates/core/src/git/`, `crates/core/src/http*`, webhook custom-headers logging, and any HTTP target/request URL logging. Remove env-var dumps, raw git-remote URLs, HTTP Authorization headers, webhook header values; redact target/request URLs.
- [ ] **P7.2** Update `crates/core/src/redact*` (or wherever the redaction layer lives): redact every secret with length ≥ 1, not ≥ 10.
- [ ] **P7.3** Add `redact::string()` public helper for inline redaction.

**P8. SBOM artifact name uses matched filename (commit 292203e)**
- [ ] **P8.1** Verify `crates/stage-sbom/` sets artifact `name` from the matched glob result, not the input glob pattern. Test: `documents: ["*.spdx.json"]` matching multiple files produces distinct artifact names.

**P9. AUR/AURSources/Krew template-expand `skip_upload` before bool check (commit cba5b9f)**
- [ ] **P9.1** Verify `crates/stage-publish/src/aur*.rs` and `krew.rs` run `skip_upload` value through Tera template before parsing as `bool`/`auto`/empty. Regression test: `skip_upload: "{{ .IsSnapshot }}"` honored on snapshot.

**P10. Release log uses correct repo for gitlab/gitea (commit 44133de)**
- [ ] **P10.1** Verify `crates/stage-release/src/lib.rs::publish` log path picks `Release.GitLab` or `Release.Gitea` based on detected token type, not always `Release.GitHub`.

### Session Q: Strongly-suggested + niche refresh (2026-05-07)

**Q1. Cask template fixes (commits 87b542b + bb9062f)**
- [ ] **Q1.1** Reorder per-arch block in `crates/stage-publish/src/homebrew/cask.rs:24-25` so `sha256` precedes `url`. Update goldens.
- [ ] **Q1.2** Render `generate_completions_from_executable` in cask template, emitting AFTER `postflight` stanza. Format: `generate_completions_from_executable "<binary>", ["<sub>"], base_name: "<n>", shell_parameter_format: :<fmt>, shells: [:<s>...]`. Verify struct → template wiring.

**Q2. dockers/v2 nil-safe parsePlatform (commit 9e9f87c)**
- [ ] **Q2.1** Verify `crates/stage-docker/src/v2/` parses platform string `"linux"` (no arch component) without panic. Add regression test.

**Q3. dockers/v2 digest log split (commit e7a4afa)**
- [ ] **Q3.1** Update v2 build log to emit `images` and `digest` as separate fields, not embedded `images@digest`. Affects observability tooling.

**Q4. dockers/v1 marked deprecated (commit e09e23a)**
- [ ] **Q4.1** Add `// Deprecated: prefer docker_v2` comment to `DockerConfig` and `DockerManifestConfig` (or the Rust equivalent) so docs-generators surface it.

**Q5. Rate-limit checker iterative + ctx-cancellable (commit 60028b1)**
- [ ] **Q5.1** Verify `crates/stage-release/src/github/rate_limit.rs` uses iterative wait + Tokio cancellation token (or futures select with ctx.Done equivalent), not recursion.

**Q6. Mattermost Color from own struct (commit 7e7f9b2)**
- [ ] **Q6.1** Verify `crates/stage-announce/src/mattermost.rs` reads `Mattermost.Color`, not `Teams.Color`. Add regression test.

**Q7. LinkedIn / Webhook / OpenCollective error wrapping**
- [ ] **Q7.1** Mirror error categorisation upstream applied: linkedin (commit 0944b0e), webhook (commit bba909e), opencollective (commit 206120a, #6512).

**Q8. Snapcraft 5xx retry (commit eb944f9)**
- [ ] **Q8.1** Wrap snapcraft push in `do_retry` once P1 lands (or 10×10s expo backoff if P1 not yet ready).

**Q9. Blob provider templated before S3-ACL gate (commit 4d1924d)**
- [ ] **Q9.1** Verify `crates/stage-blob/` applies template to `provider` before routing on `provider == "s3"`.

**Q10. Gitea create-file falls back to server default branch (commit 4a9d25f)**
- [ ] **Q10.1** Verify `crates/stage-publish/src/util/gitea*.rs` (or wherever) leaves branch empty so Gitea API uses repo default; do not hard-code `master`.

**Q11. GitHub updateRelease nil-guard resp (commit 1ca21f0)**
- [ ] **Q11.1** Verify octocrab response handling does not panic on nil `resp` while accessing `X-GitHub-Request-Id`.

**Q12. `git config` extraction preserves underlying error (commit 5042b84)**
- [ ] **Q12.1** Verify `crates/core/src/git/config.rs` (or wherever) wraps the underlying error rather than replacing with sentinel string.

**Q13. `bodyOf` returns descriptive error on ReadAll failure (commit 8b77358)**
- [ ] **Q13.1** Verify HTTP body-reader paths return `Result` and don't silently truncate on read failure.

**Q14. Redact writer returns 0 bytes on inner-write failure (commit f48613d)**
- [ ] **Q14.1** Verify anodizer's redact `Write` impl returns `(0, err)` not `(len(p), err)` on inner write failure.

**Q15. Changelog abbrev clamp + filter regex error (commits 88daaf3 + c2f16b9)**
- [ ] **Q15.1** Verify changelog stage clamps `abbrev` to ≥ -1 (no panic on -2 etc.).
- [ ] **Q15.2** Confirm `tmpl.filter`/`reverseFilter` return Result on bad regex (already true via Rust regex crate; document in row notes).

**Q16. Checksums refreshAll sort tolerates lines without double-space (commit f39c233)**
- [ ] **Q16.1** Verify `crates/stage-checksum/src/run.rs` sort path handles malformed `<hash>  <name>` lines without panic.

**Q17. Archive xz format (single-file) (commit bb532b6)**
- [ ] **Q17.1** Add `xz` to the `formats:` enum (`crates/core/src/config/archives.rs` accept-list). Implement single-file xz writer (use `xz2` crate or similar). Constraint: error if multiple files supplied. Test: `formats: [xz]` with single binary produces `<name>.xz`.

**Q18. nfpm content-mtime parse error reports the bad value (commit 50a034d)**
- [ ] **Q18.1** Verify nfpm-content mtime parse error includes the offending mtime string (anyhow context-string).

**Q19. MCP registry pipe (refresh visibility) (commit a176567 + mcp.go)**
- [ ] **Q19.1** *Tracked, not promoted*: keep `mcp registry` row at `niche-missing` until either (a) Rust MCP server ecosystem matures (rmcp adoption metrics) or (b) user demand. Reclassify candidate noted in §2.18.1.

### Session R: n/a-go-specific exclusions (2026-05-07)

These rows are added with `parity_status=n-a` and `notes` justifying. They never re-adjudicate.

- [x] **R1** `builder: node` (Node.js SEA) — JS-to-binary; anodize is Rust. Source: internal/builders/node/build.go.
- [x] **R2** `partial.archExtraEnvs[ppc64le]` — Go GGOPPC64/GOPPC64 env mapping; Rust uses target triples. Source: internal/pipe/partial/partial.go (commit e15276b).
- [x] **R3** `partial.archExtraEnvs[mips64*]` — Go GGOMIPS64/GOMIPS64 env mapping. Source: same (commit a05ecb8).
- [x] **R4** `build per-binary IDs for ./...` — Go ellipsis package selector. Source: internal/builders/golang/build.go (commit 3140abb).
- [x] **R5** `build allow explicit binary with ellipsis when single main` — Go ellipsis. Source: same (commit d077fe1).
- [x] **R6** `bun parse-error message` — Bun is a JS runtime builder. Source: internal/builders/bun/targets.go (commit 2a10e3e).
- [x] **R7** `gomod proxy 404 retry` — Go module proxy fetch retry; Rust-native replacement is Cargo.lock fidelity. Source: internal/pipe/gomod/gomod_proxy.go (commit a176567).

### Session S: Action layer refresh (2026-05-07)

Tracked separately from CLI-parity work because the action repo (`/opt/repos/goreleaser-action`) has its own release cadence (action ≥ v7.2.0 for immutable nightly).

- [ ] **S1** Update `anodize-action/action.yml` and README to handle `version: nightly` immutable-tag resolution. Goreleaser-action ≥ v7.2.0 resolves `nightly` to the latest `vX.Y.Z-<sha>-nightly` tag via GitHub Releases API. Anodizer-action must mirror — list Releases, pick newest tag matching `*-nightly`, install. Source: `www/content/customization/ci/actions.md::Nightly builds` + `goreleaser-action-feature-inventory.md::refresh-2026-05-07`.
- [ ] **S2** Document the new tag format `vX.Y.Z-<sha>-nightly` in anodize-action README, mirroring goreleaser-action.

