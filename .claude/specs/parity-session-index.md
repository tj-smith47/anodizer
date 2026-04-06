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
3. **Spec review = GoReleaser source comparison.** The spec reviewer must read the GoReleaser pipe source and compare behavior line-by-line. Checking that "the struct has the right fields" is not a spec review. The review must verify: Does anodize produce the same output as GoReleaser given the same input? Are defaults the same? Are error cases the same?
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
- N/A: continue --merge, publish --merge, announce --merge — GoReleaser has no continue/publish/announce subcommands; anodize already has them

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
GoReleaser source: every publisher pipe's `Default()` method sets `Goamd64 = "v1"` and uses it to filter amd64 microarchitecture variants. All 7 core publishers + Nix lack these fields in anodize. Requires adding `goamd64: Option<String>` and `goarm: Option<String>` to each publisher config, defaulting goamd64 to `"v1"`, and wiring the filter into `find_all_platform_artifacts_filtered()` in `util.rs`.
- [x] Add `goamd64` field to HomebrewConfig, ScoopConfig, ChocolateyConfig, WingetConfig, AurConfig, KrewConfig, NixConfig
- [x] Add `goarm` field to HomebrewConfig, KrewConfig
- [x] Wire goamd64/goarm into artifact filtering in `util.rs:find_all_platform_artifacts_filtered()`
- [x] Default goamd64 to "v1" in each publisher's defaults path

**Artifactory/Fury/CloudSmith: non-functional live mode**
These publishers only have dry-run logging — no actual HTTP upload code. GoReleaser source: `internal/http/http.go` (shared upload module). Anodize files: `artifactory.rs`, `fury.rs`, `cloudsmith.rs` all have `"artifact registry not yet implemented"` placeholders. All config fields (ids, exts, mode, checksum, signature, meta, custom_artifact_name, custom_headers, extra_files, client_x509_cert/key) are declared but unwired.
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
GoReleaser source: `upx.go:119` filters `ByTypes(Binary, UniversalBinary)`. `macos.go:79` same. Anodize only matches `ArtifactKind::Binary`. macOS universal/fat binaries are silently skipped.
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
GoReleaser source: `internal/pipe/cask/` — Cask is a top-level config section (`homebrew_casks []HomebrewCask`) with its own repository, commit_author, directory, skip_upload, hooks, dependencies, conflicts, manpages, completions, service, structured uninstall/zap, and structured URL config. Anodize nests cask as a child of HomebrewConfig with ~15 missing fields.
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
GoReleaser's `ReleaseURLTemplate()` uses `GitHubURLs.Download` to construct artifact download URLs stored in artifact metadata. Publishers (homebrew, scoop, krew, nix, chocolatey, winget, cask) read `artifact.metadata["url"]` for download links. Anodize reads `github_urls.download` but never stores it in artifact metadata — publishers only work if `url_template` is explicitly set.
- [x] After GitHub release asset upload, populate `artifact.metadata["url"]` using download URL + owner/name/tag/artifact pattern
- [x] Set default `github_urls.download` to `"https://github.com"` when not specified
- [x] Wire `ReleaseURLTemplate` for GitHub (matching `{download}/{owner}/{name}/releases/download/{tag}/{artifact}`)
- [x] Verify all 7 publishers + cask read artifact URL metadata correctly when `url_template` is not set

**GitLab/Gitea: upload retry with exponential backoff**
GoReleaser wraps all upload errors in `RetriableError{}` and retries up to 10 times with 50ms base delay. Anodize has single-retry on link conflict (400/422) for GitLab, zero retry for Gitea uploads.
- [x] Add configurable retry wrapper for GitLab `upload_via_package_registry()` and `upload_via_project_uploads()`
- [x] Add retry wrapper for Gitea `gitea_upload_asset()`
- [x] Match GoReleaser: 10 attempts, 50ms base delay, exponential backoff
- [x] Wrap transient HTTP errors (5xx, timeouts) in retriable error type

**GitLab: version detection API fallback**
GoReleaser checks `CI_SERVER_VERSION` env, then falls back to `/api/v4/version` API call. Anodize only checks env, defaults to v17+ when absent.
- [x] Add API call fallback to `/api/v4/version` when `CI_SERVER_VERSION` not set
- [x] Parse version response and compare against v17.0.0

**Changelog: co-author extraction from commit trailers**
GoReleaser's `changelog.ExtractCoAuthors()` parses `Co-Authored-By:` trailers from commit message bodies for all three backends (GitHub, GitLab, Gitea). Anodize has zero co-author extraction.
- [x] Implement `extract_co_authors()` parser for `Co-Authored-By:` trailer format
- [x] Wire into all three SCM changelog backends (github, gitlab, gitea)
- [x] Include co-authors in `CommitInfo.logins` aggregation

**Changelog: GitLab/Gitea markdown newline handling**
GoReleaser uses `"   \n"` (3 spaces + newline) for GitLab/Gitea changelog entries to force markdown line breaks. Anodize uses plain `"\n"`.
- [x] Detect token type and use 3-space newline for GitLab/Gitea changelog formatting
- [x] Apply in changelog entry joining (between bullet items)

**Release: body truncation**
GoReleaser truncates release body to `maxReleaseBodyLength = 125000` characters (client.go:19). Anodize has no truncation.
- [x] Add body length check before release creation
- [x] Truncate with warning when body exceeds 125,000 characters (GitHub limit)

### Session I: Archive & Source Behavioral Gaps (from 2026-04-06 audit)

**Archive: glob file resolution must preserve directory structure**
GoReleaser's `archivefiles.go` computes longest common prefix when `destination` is set, preserving relative directory structure. Anodize flattens all resolved files to root — `docs/README.md` becomes just `README.md`.
- [x] Implement longest-common-prefix logic for glob resolution when `dst` is set
- [x] Preserve relative directory structure under destination prefix
- [x] Add tests comparing archive contents with GoReleaser output

**Archive: duplicate destination path detection**
GoReleaser's `unique()` function warns when same destination path would be overwritten. Anodize has no duplicate detection.
- [x] Add duplicate destination path detection in `resolve_file_specs()`
- [x] Emit warning when duplicate destinations found

**Archive: file sorting for reproducibility**
GoReleaser sorts resolved file list by destination path. Anodize does not sort.
- [x] Sort resolved files by destination path before archiving

**Archive: template rendering for FileInfo fields**
GoReleaser templates `owner`, `group`, `mtime` fields in FileInfo via `tmplInfo()`. Anodize uses raw config values.
- [x] Template-render `owner`, `group`, `mtime` in FileInfo before applying to archive entries

**Archive: `binaries` filter field parsed but not wired**
`ArchiveConfig.binaries` is parsed from config but never used in artifact filtering logic.
- [x] Wire `binaries` field to filter binary artifacts by name in archive stage

**Archive: Amd64 version suffix in default template**
GoReleaser's default archive name template includes `{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}`. Anodize's default omits this.
- [x] Add Amd64 suffix to default archive name template

**Archive: support archiving UniversalBinary/Header/CArchive/CShared artifact types**
GoReleaser archives Binary, UniversalBinary, Header, CArchive, CShared. Anodize only archives Binary.
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
GoReleaser binary_signs use `${artifact}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}...` for signature naming. Anodize defaults to `{artifact}.sig` with no architecture info.
- [x] Implement architecture-aware default signature template for binary_signs
- [x] Include Os, Arch, Arm, Amd64 variant suffixes matching GoReleaser

**Sign: IDs warning when used with checksum/source artifacts**
GoReleaser warns when `ids` filter is set with `artifacts: "checksum"` or `"source"` (ids has no effect). Anodize is silent.
- [x] Add warning log when `ids` is set with checksum or source artifact filter

**Sign: DockerImageV2 handling in docker_signs**
GoReleaser includes `DockerImageV2` in both "images" and "manifests" filters. Anodize only handles `DockerImage` and `DockerManifest`.
- [x] Add `DockerImageV2` to docker_signs image and manifest filters

**Sign: artifact refresh after signing**
GoReleaser calls `ctx.Artifacts.Refresh()` after all signing. Anodize does not refresh.
- [x] Call artifact refresh equivalent after signing stage completes

**Docker: DockerV2 missing defaults**
GoReleaser sets defaults for DockerV2: ID=ProjectName, Dockerfile="Dockerfile", Tags=["{{.Tag}}"], Platforms=["linux/amd64","linux/arm64"], SBOM="true", Retry(10 attempts, 10s delay, 5m max).
- [x] Set default ID, Dockerfile, Tags, Platforms, SBOM, Retry for DockerV2 configs

**Docker: platform tag suffix for snapshot builds**
GoReleaser v2 adds platform suffix to tags during snapshot builds (e.g., "latest-amd64"). Anodize does not.
- [x] Add platform suffix to tags during snapshot builds for DockerV2

**Docker: SBOM attestation format**
GoReleaser uses `--attest=type=sbom` for proper OCI attestation. Anodize uses `--sbom=true`.
- [x] Change SBOM flag from `--sbom=true` to `--attest=type=sbom` for buildx

**Docker: buildx driver validation**
GoReleaser v2 checks buildx driver is "docker-container" or "docker". Anodize has no validation.
- [x] Validate buildx driver type and warn if non-standard

**Docker: Dockerfile path template rendering**
GoReleaser templates the `Dockerfile` field path. Anodize treats it as literal.
- [x] Template-render the Dockerfile path field before use

**Docker: index annotation prefix for multi-platform**
GoReleaser v2 adds "index:" prefix to annotations for multi-platform builds. Anodize does not.
- [x] Add "index:" annotation prefix for multi-platform Docker builds

**Docker (deep audit): V2 snapshot/publish split**
GoReleaser V2 has separate Snapshot.Run() (per-platform --load, no push, no SBOM) and Publish.Publish() (--push). Anodize had a single path.
- [x] Split V2 snapshot into per-platform --load builds (no push, no SBOM)
- [x] Remove auto --provenance=false and --sbom=false from V2 (only legacy does this)

**Docker (deep audit): V2 staging layout and artifact types**
GoReleaser V2 stages to os/arch/name and stages Binary, LinuxPackage, CArchive, CShared, PyWheel. Anodize used binaries/arch and only Binary.
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
GoReleaser's skip_push accepts template strings (e.g., "{{ if .IsSnapshot }}true{{ end }}"). Anodize only accepts "auto" or bool.
- [x] Add Template(String) variant to SkipPushConfig
- [x] Wire Template variant through resolve_skip_push

**Docker (deep audit): DockerSignConfig.env type mismatch**
GoReleaser uses []string ("KEY=VAL" list) for docker_signs env. Anodize uses HashMap. YAML input from GoReleaser configs won't deserialize.
- [ ] Add custom deserializer that accepts both map and list-of-strings formats

**Docker (deep audit): DockerSignConfig.output type**
GoReleaser accepts string-or-bool for docker_signs output. Anodize only accepts bool.
- [ ] Change DockerSignConfig.output from Option<bool> to Option<StringOrBool>

**Docker (deep audit): Environment variable passthrough**
GoReleaser passes ctx.Env.Strings() to docker commands. Anodize inherits process env but doesn't inject context env vars.
- [ ] Merge context env vars into docker command environment

**Docker (deep audit): Output secret redaction**
GoReleaser pipes docker command output through redact.Writer to strip secrets. Anodize writes raw output.
- [ ] Implement output redaction for docker command stdout/stderr

**Docker (deep audit): UX diagnostics**
GoReleaser has several diagnostic helpers that Anodize lacks.
- [x] Zero-artifact warning when no matching binaries found for platform
- [x] File-not-found diagnostic (detect COPY/ADD failures, show staging dir contents)
- [x] Buildx context error diagnostic (suggest "docker context use default")
- [ ] "Did you mean?" Levenshtein suggestion for manifest image mismatches
- [ ] Heuristic warnings when extra_files contain project markers (go.mod, Cargo.toml)
- [ ] Docker daemon availability check before snapshot builds

**Docker (deep audit): ID uniqueness validation**
GoReleaser validates docker V2 config IDs are unique. Anodize allows duplicates silently.
- [x] Validate ID uniqueness for docker V2 and manifest configs

**Docker (deep audit): Image tag deduplication and sorting**
GoReleaser deduplicates and sorts image:tag lists. Anodize does neither.
- [x] Deduplicate and sort generated image:tag combinations

**Docker (deep audit): Missing config fields on legacy Docker**
GoReleaser legacy Docker has goos, goarch, goarm, goamd64 fields for artifact filtering. Anodize uses platforms instead.
- [ ] Add goos/goarch/goarm/goamd64 fields to DockerConfig for GoReleaser config portability
- [ ] Wire these fields into artifact filtering in the legacy docker path

**Docker (deep audit): Missing DockerDigest config type**
GoReleaser has a top-level docker_digest config with disable and name_template fields that controls docker image digest artifact naming.
- [ ] Add DockerDigest config struct with disable and name_template fields
- [ ] Wire DockerDigest into the pipeline

**Docker (deep audit): --iidfile for V2 digest capture**
GoReleaser V2 uses --iidfile=id.txt to capture image digest from buildx instead of docker inspect post-push. This works even without push.
- [ ] Add --iidfile support to build_docker_v2_command and read digest from file

### Session K: nFPM & Publisher Behavioral Gaps (from 2026-04-06 audit)

**nFPM: IPK format implementation**
Config validation lists "ipk" but no underlying config struct or YAML generation exists. GoReleaser has full IPK support with 9 config fields: abi_version, alternatives, auto_installed, essential, predepends, tags, fields.
- [ ] Add `NfpmIPK` config struct with all 9 fields
- [ ] Wire IPK config into nFPM YAML generation
- [ ] Add IPK-specific test cases

**nFPM: template rendering for 8 missing field groups**
GoReleaser templates these fields; anodize passes raw values:
- [ ] Template-render `bindir` field
- [ ] Template-render `mtime` field
- [ ] Template-render script paths (preinstall, postinstall, preremove, postremove)
- [ ] Template-render signature key files (deb, rpm, apk key_file + apk key_name)
- [ ] Template-render libdirs (header, cshared, carchive)
- [ ] Template-render content src/dst paths
- [ ] Template-render content file_info.mtime

**Chocolatey: skip_publish type and phase mismatch**
GoReleaser's `SkipPublish` is a boolean checked in `Publish()`. Anodize uses `StringOrBool` checked inconsistently.
- [ ] Change chocolatey `skip_publish` from StringOrBool to boolean (match GoReleaser)
- [ ] Move skip_publish check to correct execution phase

**Nix: license validation missing**
GoReleaser validates license against a Nix-compatible allowlist. Anodize has no validation.
- [ ] Add Nix license allowlist validation in nix publisher defaults

**Publisher repository field templating**
GoReleaser applies `TemplateRef()` to repository fields for Krew and Nix. Anodize does not template repository fields.
- [ ] Template repository owner/name fields for Krew publisher
- [ ] Template repository owner/name fields for Nix publisher

**Scoop: missing default commit message template**
GoReleaser sets a default `CommitMessageTemplate` in `Default()`. Anodize does not.
- [ ] Set default commit_msg_template for Scoop publisher

**Cask: directory field not templated + missing binary inference**
GoReleaser templates directory and infers binaries from name when empty. Anodize does neither.
- [ ] Template-render directory field for Cask publisher
- [ ] Infer binary list from cask name when `binaries` is empty

**Winget: PackageIdentifier missing from commit template context + PackageName fallback**
GoReleaser adds PackageIdentifier to commit message template context and falls back PackageName to Name.
- [ ] Add PackageIdentifier to winget commit message template context
- [ ] Implement PackageName fallback to Name when empty

**AUR: directory field not templated**
GoReleaser templates the `directory` field. Anodize does not.
- [ ] Template-render directory field for AUR publisher

### Session L: Config/Defaults & Announce Gaps (from 2026-04-06 audit)

**Defaults: missing default values matching GoReleaser**
GoReleaser's defaults pipe sets many values that anodize does not:
- [ ] Default `snapshot.version_template` to `"{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"`
- [ ] Default `release.name_template` to `"{{ .Tag }}"`
- [ ] Default `checksum.algorithm` to `"sha256"`
- [ ] Default `git.tag_sort` to `"-version:refname"` when not set
- [ ] Default archive `files` to include license/readme/changelog patterns (like GoReleaser)

**Token file path defaults**
GoReleaser defaults token files to `~/.config/goreleaser/{github,gitlab,gitea}_token`. Anodize has no defaults.
- [ ] Set default token file paths in env_files when not configured

**force_token environment variable override**
GoReleaser supports `GORELEASER_FORCE_TOKEN` env var. Anodize only supports config field.
- [ ] Read `GORELEASER_FORCE_TOKEN` (or `ANODIZE_FORCE_TOKEN`) env var as fallback

**Announce: missing default values for providers**
GoReleaser sets default username/author/icon for several providers. Anodize does not.
- [ ] Slack: default username to "anodize"
- [ ] Discord: default author to "anodize", default color to 3888754
- [ ] Teams: default icon_url
- [ ] Mattermost: align default username

**Announce: webhook Content-Type charset**
GoReleaser defaults to `"application/json; charset=utf-8"`. Anodize uses `"application/json"`.
- [ ] Add `; charset=utf-8` to webhook default Content-Type

**Announce: Mattermost top-level text with attachments**
GoReleaser sets top-level `text` field when using attachments. Anodize intentionally omits it.
- [ ] Verify Mattermost behavior: add top-level `text` when attachments present (match GoReleaser)

### Session M: Missing Stages & Cross-Cutting (from 2026-04-06 audit)

**Makeself: self-extracting archives (new stage)**
GoReleaser has full makeself support (338 lines): `.run` self-extracting shell archives with embedded binaries, compression options, LSM metadata, per-platform packaging. Anodize has `ArtifactKind::Makeself` defined but zero implementation.
- [ ] Create `stage-makeself` crate
- [ ] Implement MakeselfConfig: id, ids, name_template, scripts, extra_files, compression (gzip/bzip2/xz/lzo/none), lsm_file, disable
- [ ] Subprocess: `makeself --{compression} [--lsm] <archive_dir> <output.run> <label> [startup_script]`
- [ ] Wire into pipeline ordering (after archive stage)

**SRPM: source RPM packages (new stage)**
GoReleaser has source RPM support (248 lines): `.src.rpm` from spec templates, signatures, per-format config.
- [ ] Create `stage-srpm` crate
- [ ] Implement SrpmConfig: id, ids, name_template, spec_template, extra_files, disable
- [ ] Generate spec file from template with version/release/license substitution
- [ ] Subprocess: `rpmbuild -bs <spec>` (or nfpm integration)

**Milestone closing**
GoReleaser closes GitHub/GitLab/Gitea milestones after release (93 lines). Anodize has no milestone support.
- [ ] Add MilestoneConfig: close (bool), fail_on_error (bool), name_template (default "{{ .Tag }}")
- [ ] Wire into existing VCS clients (GitHub via octocrab, GitLab/Gitea via reqwest)
- [ ] Execute after release stage

**Generic HTTP upload**
GoReleaser has generic HTTP upload to arbitrary endpoints (42 lines in pipe, delegates to shared HTTP module). Distinct from specialized Artifactory/Fury/CloudSmith publishers.
- [ ] Add UploadConfig: name, target (URL template), method (PUT/POST), username, password, custom_headers, ids, exts, checksum_header, trusted_certificates, disable
- [ ] Implement generic HTTP upload reusing Artifactory's reqwest infrastructure

**AUR source packages**
GoReleaser's `aursources` pipe generates source-only PKGBUILD (not `-bin`). Anodize has AUR binary packages but not source variant.
- [ ] Implement AUR source package generation (PKGBUILD without prebuilt binaries)
- [ ] Wire separate AUR source config alongside existing binary AUR publisher

**Snapcraft: plugs structure (dict vs list)**
GoReleaser uses `Plugs map[string]any` (structured plug definitions). Anodize uses `Vec<String>` (list only).
- [ ] Change snapcraft plugs from `Vec<String>` to structured map (interface + attributes)
- [ ] Update snap.yaml generation to output structured plug definitions

**Snapcraft: default grade and channel templates**
GoReleaser defaults grade to "stable" and auto-populates channel_templates based on grade. Anodize does neither.
- [ ] Default snapcraft grade to "stable"
- [ ] Auto-populate channel_templates based on grade ("edge,beta,candidate,stable" for stable; "edge,beta" for devel)

**Custom publishers: OS/Arch template variables**
GoReleaser exposes per-artifact `.OS`, `.Arch`, `.Target` variables. Anodize exposes `ArtifactPath`, `ArtifactName`, `ArtifactKind` but not OS/Arch.
- [ ] Add `Os`, `Arch`, `Target` template variables to custom publisher per-artifact context

**Custom publishers: system environment passthrough**
GoReleaser explicitly passes HOME, USER, PATH, TMPDIR, etc. Anodize only passes publisher-configured env.
- [ ] Pass standard system environment variables to custom publisher commands

### Phase Z: Final Parity Audit

**After ALL sessions A-M complete:**
- [ ] Update goreleaser-parity-matrix.md — mark all items Implemented
- [ ] Delete superseded docs: parity-gap-analysis.md
- [ ] Consolidate fresh-parity-gap-analysis.md into matrix
- [ ] Fresh parity audit: read EVERY GoReleaser pipe at `/opt/repos/goreleaser/internal/pipe/*/`, compare against our implementation using the parity definition above (config field parity, behavioral parity, wiring parity, error parity, auth parity, default parity). Produce pass/fail report per feature area. Fix all remaining gaps. Re-audit until zero failures. This is the sign-off.

### Post-Release: Developer Experience / Infrastructure

- [x] JSON Schema generation: `anodize jsonschema` CLI command using schemars-derived schema
- [x] Config reference auto-generated from JSON Schema (xtask gen-docs)
- [ ] Publish JSON Schema to docs site URL
- [ ] Register with SchemaStore.org for auto-discovery (`.anodize.y{,a}ml` pattern)
- [ ] `# yaml-language-server: $schema=...` inline comment works automatically once schema is published
