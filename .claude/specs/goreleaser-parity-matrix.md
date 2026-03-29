# GoReleaser Parity Matrix — Session 6 Audit

> Generated 2026-03-28 from GoReleaser v2.x docs vs anodize current state (1193 tests)

## Legend
- **Implemented**: Full parity with GoReleaser OSS behavior
- **Partial**: Config field exists but behavior is incomplete
- **Missing**: Should be in v0.1 — users coming from GoReleaser would notice
- **Omitted**: Intentionally excluded (with reason)
- **N/A**: Not applicable to Rust ecosystem

---

## 1. Builds

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Multiple builders (go/rust/zig/deno/bun) | Yes | Rust only | N/A | Rust-focused tool |
| `builds[].binary` | Yes | Yes | Implemented | |
| `builds[].targets` | Yes | Yes | Implemented | Rust target triples |
| `builds[].flags` | Yes | Yes | Implemented | |
| `builds[].features` | N/A (Go tags) | Yes | Implemented | Rust-specific |
| `builds[].no_default_features` | N/A | Yes | Implemented | Rust-specific |
| `builds[].env` (per-target) | Yes | Yes | Implemented | |
| `builds[].copy_from` | N/A (Go) | Yes | Implemented | |
| `builds[].reproducible` | `mod_timestamp` | Yes | Implemented | SOURCE_DATE_EPOCH + remap-path-prefix |
| `builds[].skip` | Yes | No | **Missing** | Skip a build config entirely |
| `builds[].id` | Yes | No | **Missing** | For cross-referencing from archives/signs |
| `builds[].hooks.pre/post` | Yes (structured) | No | **Missing** | Per-build hooks |
| `builds[].dir` | Yes | No | Omitted | Not needed — Cargo workspace handles this |
| Cross-compilation strategy | `tool` field | `CrossStrategy` enum | Implemented | Different API, same result |
| `defaults.targets` | Yes (goos/goarch) | Yes | Implemented | |
| `defaults.ignore` | Yes | Yes | Implemented | os+arch exclusion |
| `defaults.overrides` | Yes | Yes | Implemented | Per-target overrides |
| `--parallelism` | Yes | Yes | Implemented | |
| `--single-target` | Yes | Yes | Implemented | |
| Universal binaries (macOS lipo) | Yes | Yes | Implemented | |
| UPX compression | Yes | Yes | Implemented | |
| Version sync | N/A (Go) | Yes | Implemented | Rust-specific |
| Binstall metadata | N/A (Go) | Yes | Implemented | Rust-specific |
| gomod proxy | Yes | N/A | N/A | Go-specific |
| `no_unique_dist_dir` | Yes | No | Omitted | Low value |
| `no_main_check` | Yes | No | Omitted | Go-specific |
| `buildmode` (c-shared etc) | Yes | Yes | Implemented | cdylib/staticlib/wasm |

## 2. Archives

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `archives[].format` | Yes | Yes | Implemented | |
| `archives[].formats` (plural) | Yes (v2+) | No | **Missing** | Produce multiple formats per config |
| `archives[].name_template` | Yes | Yes | Implemented | |
| `archives[].wrap_in_directory` | Yes (bool or string) | Yes (string) | Implemented | |
| `archives[].format_overrides` | Yes | Yes | Implemented | |
| `archives[].files` (glob strings) | Yes | Yes | Implemented | |
| `archives[].files` (objects w/ src/dst/info) | Yes | No | Omitted | Low value for v0.1 |
| `archives[].id` | Yes | No | **Missing** | For cross-referencing |
| `archives[].ids` (build filter) | Yes | No | **Missing** | Filter which builds to include |
| `archives[].meta` (no-binary archives) | Yes | No | Omitted | Niche |
| `archives[].builds_info` (permissions) | Yes | No | Omitted | Niche |
| `archives[].strip_binary_directory` | Yes | No | Omitted | Low value |
| `archives[].allow_different_binary_count` | Yes | No | Omitted | Validation detail |
| `archives[].hooks` | Yes | No | Omitted | Niche |
| tar.gz format | Yes | Yes | Implemented | |
| tar.xz format | Yes | Yes | Implemented | |
| tar.zst format | Yes | Yes | Implemented | |
| zip format | Yes | Yes | Implemented | |
| binary (raw copy) | Yes | Yes | Implemented | |
| gz format | Yes | No | Omitted | Rarely used |
| `archives[].binaries` (filter) | N/A | Yes | Implemented | Anodize-specific |

## 3. Checksums

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `checksum.algorithm` | 14 algorithms | 7 algorithms | **Partial** | Missing: sha3-*, blake3, crc32, md5 |
| `checksum.name_template` | Yes | Yes | Implemented | |
| `checksum.disable` | Yes | Yes | Implemented | |
| `checksum.ids` | Yes | Yes | Implemented | |
| `checksum.extra_files` | Yes (objects) | Yes (strings) | **Partial** | Missing name_template on extra_files |
| `checksum.split` | Yes | No | **Missing** | One checksum file per artifact |

## 4. Release

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `release.github` (owner/name) | Yes | Yes | Implemented | |
| Auto-detect from git remote | Yes | Yes | Implemented | |
| `release.draft` | Yes | Yes | Implemented | |
| `release.prerelease` (auto/bool) | Yes | Yes | Implemented | |
| `release.make_latest` (auto/bool) | Yes | Yes | Implemented | |
| `release.name_template` | Yes | Yes | Implemented | |
| `release.header` / `release.footer` | Yes | Yes | Implemented | |
| `release.extra_files` | Yes (objects) | Yes (strings) | **Partial** | Missing name_template |
| `release.skip_upload` | Yes | Yes | Implemented | |
| `release.replace_existing_draft` | Yes | Yes | Implemented | |
| `release.replace_existing_artifacts` | Yes | Yes | Implemented | |
| `release.disable` | Yes | No | **Missing** | Skip release creation entirely |
| `release.ids` (artifact filter) | Yes | No | **Missing** | Filter which artifacts to upload |
| `release.target_commitish` | Yes | No | Omitted | Niche |
| `release.discussion_category_name` | Yes | No | Omitted | GitHub-specific niche |
| `release.mode` (append/prepend/replace) | Yes | No | **Missing** | How to handle existing release notes |
| `release.include_meta` | Yes | No | Omitted | metadata.json upload |
| `release.use_existing_draft` | Yes | No | Omitted | Niche workflow |
| GitLab/Gitea support | Yes | No | Omitted | GitHub-only for v0.1 |

## 5. Changelog

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `changelog.disable` | Yes | Yes | Implemented | |
| `changelog.use` (git/github-native) | Yes (5 backends) | Yes (2 backends) | Implemented | git + github-native |
| `changelog.sort` | Yes | Yes | Implemented | |
| `changelog.abbrev` | Yes | Yes | Implemented | |
| `changelog.filters.exclude` | Yes | Yes | Implemented | |
| `changelog.filters.include` | Yes | Yes | Implemented | |
| `changelog.groups` (title/regexp/order) | Yes | Yes | Implemented | |
| `changelog.header` / `changelog.footer` | Release section | Changelog section | Implemented | Different location, same effect |
| `changelog.format` (commit template) | Yes | No | **Missing** | Customize commit line format |
| Nested subgroups | Yes (Pro) | No | Omitted | Pro feature |
| AI enhancement | Yes (Pro) | No | Omitted | Pro feature |

## 6. Sign

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `signs[]` (array config) | Yes | Yes | Implemented | |
| `signs[].id` | Yes | Yes | Implemented | |
| `signs[].cmd` | Yes | Yes | Implemented | |
| `signs[].args` | Yes | Yes | Implemented | |
| `signs[].artifacts` filter | Yes (10 values) | Yes (7 values) | **Partial** | Missing: installer, diskimage, sbom |
| `signs[].ids` filter | Yes | Yes | Implemented | |
| `signs[].signature` template | Yes | Yes | Implemented | |
| `signs[].stdin` / `stdin_file` | Yes | Yes | Implemented | |
| `signs[].env` | Yes | No | **Missing** | Env vars for signing command |
| `signs[].certificate` | Yes | No | **Missing** | Sigstore/cosign certificate output |
| `signs[].output` | Yes | No | Omitted | Low value |
| `signs[].if` conditional | Yes (Pro) | No | Omitted | Pro feature |
| `binary_signs[]` | Yes | No | Omitted | Covered by artifacts filter |
| `docker_signs[]` | Yes | Yes | Implemented | |
| Docker signs `ids`, `stdin`, `stdin_file` | Yes | No | **Missing** | Docker sign config incomplete |

## 7. Docker

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Multi-platform buildx | Yes | Yes | Implemented | |
| `image_templates` | Yes | Yes | Implemented | |
| `dockerfile` | Yes | Yes | Implemented | |
| `platforms` | Yes | Yes | Implemented | |
| `build_flag_templates` | Yes | Yes | Implemented | |
| `skip_push` | Yes | Yes | Implemented | |
| `extra_files` | Yes | Yes | Implemented | |
| `push_flags` | Yes | Yes | Implemented | |
| `docker[].id` | Yes | No | **Missing** | For cross-referencing |
| `docker[].ids` (build filter) | Yes | No | **Missing** | Filter which builds |
| `docker[].use` (docker/buildx/podman) | Yes | No | Omitted | Always uses buildx |
| Docker manifests | Yes | No | Omitted | Multi-image manifests (niche) |
| Docker digest file | Yes | No | Omitted | v2.12+ feature |
| Retry config | Yes | No | **Missing** | Retry on push failure |
| Labels / annotations | Yes (v2) | No | **Missing** | OCI metadata |

## 8. nFPM

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Core fields (name, formats, vendor, etc.) | Yes | Yes | Implemented | |
| `contents[]` (src/dst/type/file_info) | Yes | Yes | Implemented | |
| Scripts (pre/post install/remove) | Yes | Yes | Implemented | |
| Dependencies | Yes | Yes (per-format map) | Implemented | |
| Recommends/suggests/conflicts/replaces/provides | Yes | Yes | Implemented | |
| `file_name_template` | Yes | Yes | Implemented | |
| Format-specific overrides | Yes | Yes (JSON) | Implemented | |
| `nfpms[].id` | Yes | No | **Missing** | For cross-referencing |
| `nfpms[].ids` (build filter) | Yes | No | **Missing** | Filter which builds |
| RPM-specific fields (summary, compression, etc.) | Yes | No | Omitted | Pass via overrides |
| Deb-specific fields (triggers, breaks, etc.) | Yes | No | Omitted | Pass via overrides |
| APK-specific fields | Yes | No | Omitted | Pass via overrides |
| `epoch`, `prerelease`, `section`, `priority` | Yes | No | Omitted | Rarely used |
| Per-format signatures | Yes | No | Omitted | Niche |

## 9. Publish — Homebrew

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Tap repo (owner/name) | Yes | Yes | Implemented | |
| Formula generation | Yes | Yes | Implemented | |
| `description` | Yes | Yes | Implemented | |
| `license` | Yes | Yes | Implemented | |
| `install` | Yes | Yes | Implemented | |
| `test` | Yes | Yes | Implemented | |
| `folder` (directory in tap) | Yes | Yes | Implemented | |
| `homepage` | Yes | No | **Missing** | Separate from description |
| `dependencies` | Yes | No | **Missing** | Package dependencies |
| `conflicts` | Yes | No | **Missing** | Conflicting packages |
| `caveats` | Yes | No | **Missing** | Post-install messages |
| `skip_upload` / auto | Yes | No | **Missing** | Skip for prereleases |
| PR creation | Yes | No | Omitted | Complex, v0.2+ |
| Commit signing | Yes | No | Omitted | Niche |
| `url_template` | Yes | No | Omitted | Custom download URLs |
| `custom_block` / `post_install` | Yes | No | Omitted | Niche |
| Homebrew Casks | Yes (v2.10+) | No | Omitted | macOS GUI apps |

## 10. Publish — Scoop

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Bucket repo (owner/name) | Yes | Yes | Implemented | |
| Manifest generation | Yes | Yes | Implemented | |
| `description` | Yes | Yes | Implemented | |
| `license` | Yes | Yes | Implemented | |
| `homepage` | Yes | No | **Missing** | Project homepage |
| `persist` | Yes | No | **Missing** | Data paths persisted between updates |
| `depends` | Yes | No | **Missing** | Dependencies |
| `pre_install` / `post_install` | Yes | No | **Missing** | Install scripts |
| `shortcuts` | Yes | No | **Missing** | Start menu shortcuts |
| `skip_upload` / auto | Yes | No | **Missing** | Skip for prereleases |
| PR creation | Yes | No | Omitted | Complex, v0.2+ |

## 11. Publish — Other

| Publisher | GoReleaser | Anodize | Status |
|-----------|-----------|---------|--------|
| crates.io | N/A | Yes | Implemented (Rust-specific) |
| Chocolatey | Yes | Yes | Implemented |
| Winget | Yes | Yes | Implemented |
| AUR | Yes | Yes | Implemented |
| Krew | Yes | Yes | Implemented |
| Nix | Yes | No | Omitted |
| Snapcraft | Yes | No | Omitted |
| Custom publishers | Yes | Yes | Implemented |

## 12. Custom Publishers

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `name`, `cmd`, `args` | Yes | Yes | Implemented | |
| `ids` filter | Yes | Yes | Implemented | |
| `artifact_types` filter | N/A | Yes | Implemented | Anodize-specific |
| `env` | Yes | Yes | Implemented | |
| `dir` | Yes | No | **Missing** | Working directory |
| `checksum` (publish checksums too) | Yes | No | **Missing** | |
| `signature` (publish sigs too) | Yes | No | **Missing** | |
| `disable` conditional | Yes | No | **Missing** | Template-conditional disable |

## 13. Announce

| Provider | GoReleaser | Anodize | Status | Notes |
|----------|-----------|---------|--------|-------|
| Discord | Yes | Yes | Implemented | |
| Slack | Yes (rich) | Yes (basic) | **Partial** | Missing: channel, username, icon_emoji, blocks |
| Webhook | Yes | Yes | Implemented | |
| Telegram | Yes | Yes | Implemented | |
| Teams | Yes | Yes | Implemented | |
| Mattermost | Yes | Yes | Implemented | |
| Email/SMTP | Yes (SMTP) | Yes (sendmail) | Implemented | Different transport |
| Reddit | Yes | No | Omitted | Low priority |
| Twitter/X | Yes | No | Omitted | API unstable |
| Mastodon | Yes | No | Omitted | Low priority |
| Bluesky | Yes | No | Omitted | Low priority |
| LinkedIn | Yes | No | Omitted | Low priority |
| OpenCollective | Yes | No | Omitted | Niche |
| Discourse | Yes | No | Omitted | Niche |
| `announce.skip` | Yes | No | Omitted | Use `--skip announce` |

## 14. Templates

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| Go-style `{{ .Var }}` syntax | Native | Preprocessor | Implemented | |
| Tera/Go conditionals | `if/else/end` | `{% if %}{% endif %}` | Implemented | |
| `tolower` / `toupper` | Yes | Yes | Implemented | |
| `trimprefix` / `trimsuffix` | Yes | Yes | Implemented | |
| `lower` / `upper` / `title` / `trim` | Yes | Yes (Tera built-in) | Implemented | |
| `default` filter | Yes | Yes (Tera built-in) | Implemented | |
| `replace` filter | Yes | Yes (Tera built-in) | Implemented | |
| `envOrDefault` function | Yes | No | **Missing** | Get env var with fallback |
| `isEnvSet` function | Yes | No | **Missing** | Check if env var is set |
| `incpatch`/`incminor`/`incmajor` | Yes | No | Omitted | Niche |
| Hash functions in templates | Yes (14) | No | Omitted | Niche |
| `readFile`/`mustReadFile` | Yes | No | Omitted | Niche |
| `filter`/`reverseFilter` | Yes | No | Omitted | Niche |
| `urlPathEscape` | Yes | No | Omitted | Niche |
| `time` function | Yes | No | Omitted | Niche |
| `split`/`list`/`englishJoin` | Yes | No | Omitted | Niche |
| `dir`/`base`/`abs` | Yes | No | Omitted | Niche |
| `map`/`indexOrDefault` | Yes | No | Omitted | Niche |
| `mdv2escape` | Yes | No | Omitted | Telegram-specific |

### Template Variables

| Variable | GoReleaser | Anodize | Status |
|----------|-----------|---------|--------|
| `ProjectName` | Yes | Yes | Implemented |
| `Version` / `RawVersion` | Yes | Yes | Implemented |
| `Tag` | Yes | Yes | Implemented |
| `Major` / `Minor` / `Patch` | Yes | Yes | Implemented |
| `Prerelease` | Yes | Yes | Implemented |
| `FullCommit` / `Commit` / `ShortCommit` | Yes | Yes | Implemented |
| `Branch` | Yes | Yes | Implemented |
| `CommitDate` / `CommitTimestamp` | Yes | Yes | Implemented |
| `IsGitDirty` | Yes | Yes | Implemented |
| `GitTreeState` | Yes | Yes | Implemented |
| `PreviousTag` | Yes | Yes | Implemented |
| `IsSnapshot` / `IsNightly` / `IsDraft` | Yes | Yes | Implemented |
| `Date` / `Timestamp` / `Now` | Yes | Yes | Implemented |
| `Env.*` | Yes | Yes | Implemented |
| `Os` / `Arch` / `Binary` | Yes | Yes | Implemented |
| `ArtifactName` / `ArtifactPath` | Yes | Yes | Implemented |
| `ReleaseURL` | Yes | Yes | Implemented |
| `IsGitClean` | Yes | No | **Missing** | Inverse of IsGitDirty |
| `IsSingleTarget` | Yes | No | **Missing** | Reflects --single-target |
| `ReleaseNotes` | Yes | No | **Missing** | Changelog as template var |
| `GitURL` | Yes | No | **Missing** | Git remote URL |
| `Summary` | Yes | No | **Missing** | Git describe summary |
| `TagSubject` / `TagContents` / `TagBody` | Yes | No | **Missing** | Annotated tag fields |
| `Runtime.Goos` / `Runtime.Goarch` | Yes | No | **Missing** | Host platform |
| `ArtifactKind` | N/A | Yes | Implemented | Anodize-specific |

## 15. CLI Flags

| Flag | GoReleaser | Anodize | Status |
|------|-----------|---------|--------|
| `--config` / `-f` | Yes | Yes | Implemented |
| `--snapshot` | Yes | Yes | Implemented |
| `--auto-snapshot` | Yes | Yes | Implemented |
| `--clean` | Yes | Yes | Implemented |
| `--parallelism` / `-p` | Yes | Yes | Implemented |
| `--timeout` | Yes | Yes | Implemented |
| `--skip` | Yes | Yes | Implemented |
| `--single-target` | Yes | Yes | Implemented |
| `--release-notes` | Yes | Yes | Implemented |
| `--verbose` | Yes | Yes | Implemented |
| `--quiet` | Yes | Yes | Implemented |
| `--debug` | Yes (via verbose) | Yes | Implemented |
| `--dry-run` | Yes (release) | Yes | Implemented |
| `--draft` | Yes | No | **Missing** | Set release as draft from CLI |
| `--fail-fast` | Yes | No | Omitted | Abort on first error |
| `--release-header` / `--release-footer` | Yes | No | **Missing** | Load from file |
| `--release-notes-tmpl` | Yes | No | Omitted | Templated notes file |
| `--id` (build) | Yes | `--crate` | Implemented | Different naming |
| `--output` / `-o` (build) | Yes | No | Omitted | Copy binary to path |
| `--nightly` | N/A | Yes | Implemented | Anodize-specific |
| `--crate` | N/A | Yes | Implemented | Anodize-specific |
| `--workspace` | N/A | Yes | Implemented | Anodize-specific |
| `--token` | N/A | Yes | Implemented | Anodize-specific |

## 16. Commands

| Command | GoReleaser | Anodize | Status |
|---------|-----------|---------|--------|
| `release` | Yes | Yes | Implemented |
| `build` | Yes | Yes | Implemented |
| `check` | Yes | Yes | Implemented |
| `healthcheck` | Yes | Yes | Implemented |
| `init` | Yes | Yes | Implemented |
| `completion` | Yes | Yes | Implemented |
| `schema` / `jsonschema` | Yes | Yes | Implemented |
| `changelog` | N/A | Yes | Implemented |
| `tag` | N/A | Yes | Implemented |
| `man` | Yes | No | Omitted | Man page generation |

## 17. Global Hooks

| Feature | GoReleaser | Anodize | Status | Notes |
|---------|-----------|---------|--------|-------|
| `before.hooks` / `after.hooks` | Yes | Yes | **Partial** | Anodize: plain strings only |
| Structured hooks (cmd/dir/env/output) | Yes | No | **Missing** | Object form with fields |
| `if` conditional on hooks | Yes (v2.7+) | No | Omitted | Pro-like feature |

---

## Summary: Actionable Gaps for Session 6B

### High Priority (would surprise users)

1. **Template vars**: `IsGitClean`, `IsSingleTarget`, `ReleaseNotes`, `GitURL`, `Summary`, `TagSubject`/`TagContents`/`TagBody`, `Runtime.Goos`/`Runtime.Goarch`
2. **Template functions**: `envOrDefault`, `isEnvSet`
3. **Homebrew**: `homepage`, `dependencies`, `conflicts`, `caveats`, `skip_upload`
4. **Scoop**: `homepage`, `persist`, `depends`, `pre_install`/`post_install`, `shortcuts`, `skip_upload`
5. **Release `disable`** field
6. **Checksum `split`** mode
7. **CLI `--draft`** flag
8. **Changelog `format`** template for commit messages
9. **Sign `env`** and `certificate` fields
10. **Docker sign** missing fields: `ids`, `stdin`, `stdin_file`

### Medium Priority

11. **Structured global hooks** (cmd/dir/env/output object form)
12. **Publisher `dir`**, `checksum`, `signature`, `disable` fields
13. **Docker `id`**, retry config, labels/annotations
14. **Archive `id`/`ids`** filter fields
15. **nFPM `id`/`ids`** filter fields
16. **Release `mode`** (append/prepend/replace/keep-existing)
17. **Release `ids`** artifact filter
18. **Slack announce** enrichment (channel, username, blocks)
19. **`--release-header` / `--release-footer`** CLI flags
20. **`archives[].formats`** (plural) support
21. **Sign `artifacts` filter** missing values: `installer`, `diskimage`, `sbom`
