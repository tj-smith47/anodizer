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
- [ ] Config includes from URL (with headers) + from_file structured form — needs HTTP client, custom deserializer, GoReleaser Pro behavior match
- [x] Template files config section (id, src, dst, mode) — new stage-templatefiles crate, template rendering + artifact registration + path safety + tests
- [ ] `templated_extra_files` across sections (render file CONTENTS as templates, distinct from extra_files) — GoReleaser Pro, needs per-stage wiring: checksums, release, docker, blob, publishers, snapcraft, dmg, nsis, app_bundles
- [ ] Monorepo improvements (tag_prefix, dir) — GoReleaser Pro, needs PrefixedTag/PrefixedPreviousTag template var wiring
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

- [ ] GitLab release support
- [ ] Gitea release support
- [ ] GitHub Enterprise URLs (api/upload/download/skip_tls_verify)
- [ ] DockerHub description sync: username, secret_name, images, description, full_description (from_url/from_file)
- [ ] Artifactory publisher: name, target (template), mode (archive/binary), username/password, client_x509_cert/key, custom_headers, ids/exts
- [ ] Fury.io publisher: account, disable, secret_name, ids, formats
- [ ] CloudSmith publisher: organization, repository, ids/formats, distributions, component
- [ ] NPM publisher: name, description, license, author, access, tag
- [ ] Snapcraft publish (upload to Snap Store)
- [ ] changelog.use: gitlab backend
- [ ] changelog.use: gitea backend

### Phase Z: Final Parity Audit

**After ALL sessions A-F complete:**
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
