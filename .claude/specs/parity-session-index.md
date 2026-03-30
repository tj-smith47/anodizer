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
2. **Spec + code quality review loop.** After implementing, run spec review then code quality review. Fix ALL findings of ANY severity. Re-review. Repeat until ZERO issues/suggestions remain. That's when the task is done.
3. **Mark items done here.** Check the box in this file when an item is implemented to equal or better quality than GoReleaser.
4. **Work on master directly.** No worktrees or branches for sequential work.

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

- [ ] announce.skip (template-conditional)
- [ ] Teams icon_url
- [ ] Mattermost title_template
- [ ] Webhook expected_status_codes
- [ ] Slack blocks/attachments — verify wiring (config fields exist, behavior unverified)
- [ ] SMTP email transport (replace sendmail)
- [ ] Reddit provider
- [ ] Twitter/X provider
- [ ] Mastodon provider
- [ ] Bluesky provider
- [ ] LinkedIn provider
- [ ] OpenCollective provider
- [ ] Discourse provider

### Session C: Custom Publishers + CLI + Hooks
GoReleaser source: `internal/pipe/custompublishers/`, `cmd/`

**Custom publishers** (from parity-matrix s12)
- [ ] meta
- [ ] extra_files
- [ ] output (capture stdout)
- [ ] if (per-artifact filter)
- [ ] templated_extra_files
- [ ] Parallel per-artifact execution

**CLI flags** (from parity-matrix s15, fresh-gap B38)
- [ ] --fail-fast
- [ ] --release-notes-tmpl
- [ ] --output / -o (build)
- [ ] man command
- [ ] --prepare (publish later)
- [ ] --check --soft
- [ ] continue --merge, publish --merge, announce --merge commands (verify split/merge wiring)

**Global hooks** (from parity-matrix s17)
- [ ] if conditional on hooks

### Session D: New Stages + Subsystems
<!-- NOTE: This session is too large for a single conversation. Split into D1 (NSIS, App Bundles, SBOM rewrite, Source archive improvements) and D2 (Flatpak, Notarization, DMG, MSI, PKG) before starting. Update this plan to reflect the split. -->
GoReleaser source: `internal/pipe/flatpak/`, `internal/pipe/notary/`, `internal/pipe/sourcearchive/`, `internal/pipe/sbom/`, `internal/pipe/dmg/`, `internal/pipe/msi/`, `internal/pipe/nsis/`, `internal/pipe/pkg/`, `internal/pipe/appbundle/`

- [ ] Flatpak stage (new crate): app_id, runtime, runtime_version, sdk, command, finish_args, name_template, disable
- [ ] NSIS installer stage (new crate): id, name (template), script (template), ids, extra_files, disable (template), replace, mod_timestamp
- [ ] macOS App Bundles (new crate): id, name (template), ids, icon (.icns), bundle (reverse-DNS), extra_files, mod_timestamp
- [ ] macOS Notarization (new crate): sign.certificate/password/entitlements, notarize.issuer_id/key_id/key/wait/timeout; native variant: sign.keychain/identity/options, notarize.profile_name
- [ ] DMG stage (macOS disk images, Pro)
- [ ] MSI stage (Windows installer with Wix, Pro)
- [ ] PKG stage (macOS packages, Pro, v2.14+)
- [ ] SBOM rewrite (current config too minimal — needs cmd/args/env/artifacts/ids/documents/disable)
- [ ] Source archive improvements (prefix_template, object-form files, templated_files)

### Session E: Cross-Cutting Concerns
<!-- NOTE: This session is too large for a single conversation. Split into E1 (Config infrastructure + Pervasive patterns) and E2 (Template additions + Stage-specific extras) before starting. Update this plan to reflect the split. -->
GoReleaser source: `internal/pipe/git/`, `internal/pipe/metadata/`, `internal/pipe/env/`, `internal/pipe/defaults/`

**Config infrastructure** (from fresh-gap B1-B6)
- [ ] git.tag_sort, git.ignore_tags, git.ignore_tag_prefixes, git.prerelease_suffix
- [ ] Global metadata block (mod_timestamp, maintainers, license, homepage, description, full_description, commit_author)
- [ ] Config includes from URL (with headers); verify from_file.path behavior (config exists, wiring unverified)
- [ ] Custom template variables (.Var.*)
- [ ] Template files config section (id, src, dst, mode)
- [ ] Monorepo improvements (tag_prefix, dir)
- [ ] version schema field, report_sizes
- [ ] release.tag (Pro, template override)

**Pervasive patterns** (from fresh-gap B36-B37)
- [ ] `if` conditional across all config sections (signs, nfpms, blobs, publishers, docker, snapcrafts, sboms)
- [ ] `templated_extra_files` across all sections (archives, release, checksums, docker, blob, source, snapcraft, publishers)
- [ ] StringOrBool/template `disable` on all configs that still use bool-only (snapcrafts, sboms, etc.)

**Template additions** (from fresh-gap B31-B32)
- [ ] OSS template functions: incpatch, incminor, incmajor (version increment)
- [ ] OSS template functions: readFile, mustReadFile (file I/O)
- [ ] OSS template functions: filter, reverseFilter (regex filtering)
- [ ] OSS template functions: urlPathEscape, time (formatted UTC)
- [ ] OSS template functions: contains, list, englishJoin
- [ ] OSS template functions: dir, base, abs (path functions)
- [ ] OSS template functions: map, indexOrDefault (map functions)
- [ ] OSS template functions: mdv2escape (MarkdownV2 escaping)
- [ ] Go-style positional syntax compatibility for replace, split, contains
- [ ] Pro template functions: `in` (list membership), `reReplaceAll` (regex replace)
- [ ] Template variables: .Outputs, .PrefixedTag, .PrefixedPreviousTag, .PrefixedSummary, .IsRelease, .IsMerging, .Artifacts, .Metadata, .Var.*, .Checksums, .ArtifactExt, .ArtifactID, .Target
- [ ] nFPM-specific vars: .Release, .Epoch, .PackageName, .ConventionalFileName, .ConventionalExtension, .Format

**Stage-specific extras** (from fresh-gap B19-B20, B24, B33-B34)
- [ ] Docker extras: v2 API, annotations, SBOM, disable template, templated_dockerfile, templated_extra_files, skip_build, build_args (map form), manifest create_flags, manifest retry
- [ ] Snapcraft: hooks (top-level), missing app sub-fields (~25 fields: adapter, after, aliases, autostart, before, bus_name, command_chain, common_id, completer, desktop, extensions, install_mode, passthrough, post_stop_command, refresh_mode, reload_command, sockets, start_timeout, stop_command, stop_timeout, timer, watchdog_timeout), templated_extra_files
- [ ] nFPM extras: libdirs, changelog (YAML path), templated_contents (Pro), templated_scripts (Pro), contents file_info.owner/group template rendering
- [ ] Environment config: template expansion in env values, env_files.github_token/gitlab_token/gitea_token
- [ ] Artifacts JSON format parity (14 ArtifactKind variants vs GoReleaser's 30+ type classifications)
- [ ] Changelog Pro features: paths, title, divider, AI (use/model/prompt)

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
