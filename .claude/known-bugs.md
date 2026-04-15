# Known bugs & unfixed review findings

This file is the **source of truth for unresolved issues** in this repo.

**The user-level pre-bash hook refuses `git push` while this file has unchecked
items**, unless the command includes `--allow-unfixed`.

## Workflow

When any audit, review, test failure, or manual code-read surfaces something:

1. Add to **Active** with: `<date> <source> <short description> — <file:line if known>`
2. Fix it (or get explicit user approval to defer; record the defer in the line).
3. On fix: move to **Resolved** with the resolution date.

Sources include: code-audit, deep-audit, parity-audit, gap-analysis, dedup,
security-review, claude-md-improver, manual code review, failing tests, hook
violations, user-reported issues.

## Active

### 2026-04-15 parity-audit findings (5-batch comparison vs GoReleaser)

**Release / GitHub / Changelog**
- [ ] 2026-04-15 parity-audit: B-41 GitHub release PATCH clobbers existing draft state — `stage-release/src/lib.rs:676,2009-2028`. GoReleaser `github.go:541` does `data.Draft = release.Draft` on update. Fix: when PATCHing existing_by_tag, preserve existing.draft in json_body. Also: anodize's CREATE uses user's draft directly; GoReleaser always starts draft=true then un-drafts in publish — verify anodize publish flow un-drafts correctly.
- [ ] 2026-04-15 parity-audit: B-15 Sign subprocess env vars ($artifact/$signature/$certificate/$artifactID/$digest) not passed in child process env — only used for arg substitution. `stage-sign/src/lib.rs:615-631,739`. Fix: include these in SignJob.env_vars so `command.envs()` sets them.
- [ ] 2026-04-15 parity-audit: B-24 Changelog git-backend default uses full SHA. `{{ SHA }}` should respect abbrev config. `stage-changelog/src/lib.rs:497`. Fix: `vars.set("SHA", &short_sha)` so abbrev=7/0/-1 matches GoReleaser.
- [ ] 2026-04-15 parity-audit: B-36 Release extra_files missing-file silent skip. Should be hard error per `release_test.go:284-302`. `stage-release/src/lib.rs:477-524`.
- [ ] 2026-04-15 parity-audit: C-2 SBOM multi-document validation missing (artifacts!=any && len(documents)>1 should error). `stage-sbom/src/lib.rs:337-355`.
- [ ] 2026-04-15 parity-audit: B-22/B-23 Milestone provider resolution uses first-crate fallback; should consult `ctx.token_type` first. `cli/src/commands/release/milestones.rs:143-165,112-139`.
- [ ] 2026-04-15 parity-audit: F-1 Publisher order — package-managers run before infra publishers. GoReleaser `publish.go:46-74` runs blob/upload/artifactory/docker/ko/docker_sign/snapcraft FIRST, then release, then package-managers. `stage-publish/src/lib.rs:93-161`.
- [ ] 2026-04-15 parity-audit: B-48 SBOM binary filter doesn't dedupe UniversalBinary + per-arch Darwin binaries (3 SBOMs instead of 1). `stage-sbom/src/lib.rs:391-414`.
- [ ] 2026-04-15 parity-audit: B-1/B-2 Sign `artifacts: all` scope misalignment vs GoReleaser's `ReleaseUploadableTypes` list.
- [ ] 2026-04-15 parity-audit: B-3/B-4 Release `include_meta` — anodize reads files from disk; GoReleaser uploads `Metadata` artifact type. Also Metadata always appears regardless of include_meta flag. `stage-release/src/lib.rs:1160-1165`.
- [ ] 2026-04-15 parity-audit: B-6/B-7/B-33 Sign artifacts:none, docker_signs:none, custom-publisher skip handling uses bare `continue` instead of explicit skip memento.
- [ ] 2026-04-15 parity-audit: GitLab JOB-TOKEN safety — anodize sends `JOB-TOKEN` when `use_job_token:true` regardless of actual token value. Should only when token matches `CI_JOB_TOKEN`. `stage-release/src/gitlab.rs`.

**Publishers**
- [ ] 2026-04-15 parity-audit: K2 Krew one-binary-per-archive enforcement missing — GoReleaser `krew.go:233-236` hard-errors on >1 binary. `stage-publish/src/krew.rs:187-213,334`.
- [ ] 2026-04-15 parity-audit: D1/D2/D3 ID uniqueness validation across dockers / dockers_v2 / docker_manifests (verify — some claimed done in prior sessions).
- [ ] 2026-04-15 parity-audit: D4 Docker V2 retry logic too broad — should retry only manifest-verification errors per GoReleaser.
- [ ] 2026-04-15 parity-audit: D11 Legacy Docker missing `len(IDs) vs len(GroupByID())` cardinality check.
- [ ] 2026-04-15 parity-audit: D14 `docker manifest rm` stricter than GoReleaser (anodize bails on non-"not found"; GoReleaser ignores all).
- [ ] 2026-04-15 parity-audit: D15 `docker_digest.name_template` does not control combined-digest filename (hardcoded `digests.txt`).
- [ ] 2026-04-15 parity-audit: A11 AUR pushes current branch (relies on clone default being `master`); hardcode "master" for safety. `stage-publish/src/aur.rs:552`.
- [ ] 2026-04-15 parity-audit: P2/P10 Custom publisher default artifact filter too broad — allows all non-Metadata; GoReleaser curates 10-type list. `cli/src/commands/publisher.rs:291-332`.

**Build / Archive / Source / Checksum / UniversalBinary**
- [ ] 2026-04-15 parity-audit: B16 Checksum Refresh missing — signatures added after checksum pipe not included in checksums.txt. GoReleaser registers `Extra[ExtraRefresh]`; `stage-checksum/src/lib.rs:507-521,578-589`. Fix: re-run checksum at start of release stage for signed artifacts.
- [ ] 2026-04-15 parity-audit: B1 Archive builds_info partial user override drops default mode (0755). `stage-archive/src/lib.rs:1179-1184`.
- [ ] 2026-04-15 parity-audit: E16 strip_parent + dst collision — multiple matched files collide at same archive path. `stage-archive/src/lib.rs:506-513,1220-1222`.
- [ ] 2026-04-15 parity-audit: B34 UniversalBinary registration loses source binaries' Extras (`DynamicallyLinked`, `Abi`, `Libc`). `stage-build/src/lib.rs:515-527`.
- [ ] 2026-04-15 parity-audit: B10 Archive extra_binaries stored as CSV string; GoReleaser stores list. Template `{{ range .Binaries }}` breaks.
- [ ] 2026-04-15 parity-audit: B27 Source archive extra_files drop directory hierarchy — `docs/sub/file.txt` → `file.txt`. `stage-source/src/lib.rs:200-214`.
- [ ] 2026-04-15 parity-audit: B12/B13 Archive loses `Replaces` and `DynamicallyLinked` from source binaries (publisher filters misfire).
- [ ] 2026-04-15 parity-audit: C5 UniversalBinary ids no default to `[ID]`. `stage-build/src/lib.rs` — add Defaults pass.
- [ ] 2026-04-15 parity-audit: D11 UniversalBinaryConfig has no `id` field. `core/src/config.rs:854-865`. Add `id: Option<String>` defaulting to ProjectName; wire into metadata.
- [ ] 2026-04-15 parity-audit: B20 Split checksum sidecar metadata uses `"source"` key; GoReleaser uses `"ChecksumOf"` (`artifact.ExtraChecksumOf`). `stage-checksum/src/lib.rs:516-519`.

**Config / CLI / Pipeline**
- [ ] 2026-04-15 parity-audit: `ctx.deprecate()` exists at `core/src/context.rs:166` but is never wired at any serde alias site. Add pre-parse YAML walk in `cli/src/pipeline.rs::load_config` to detect deprecated aliases (`gemfury`, `brews[].directory`, `before.hooks`, `nfpm.builds` / `snapcraft.builds`, snapshot.name_template rename, smtp body_template, etc.) and emit deprecations.
- [ ] 2026-04-15 parity-audit: `--skip=before` not honored in release pipeline — `cli/src/commands/release/mod.rs:308-320` has no `ctx.should_skip("before")` guard.
- [ ] 2026-04-15 parity-audit: Before hooks run BEFORE git context in release — templates can't reference `{{ .Tag }}`. GoReleaser `pipeline.go:69,79` runs git.Pipe (index 2) before before.Pipe (index 7). Reorder `resolve_git_context` to come before the before-hook block in `release/mod.rs`.
- [ ] 2026-04-15 parity-audit: `anodize build` pipeline missing reportsizes + metadata.json + artifacts.json + effectiveconfig.yaml + before hooks. GoReleaser's `BuildCmdPipeline` includes them. `cli/src/commands/build.rs`.
- [ ] 2026-04-15 parity-audit: `project_name` auto-inference from `Cargo.toml` lives only in `release/mod.rs:99-111`. Extract to `helpers::infer_project_name` and call from release, build, check, continue_cmd.
- [ ] 2026-04-15 parity-audit: `tag_pre_hooks` / `tag_post_hooks` (GoReleaser Pro) absent.
- [ ] 2026-04-15 parity-audit: `config.env` accepts both map and list form; deprecate map form (GoReleaser only accepts list) or document.
- [ ] 2026-04-15 parity-audit: Nightly `name_template` default diverges from GoReleaser Pro `{{ .Version }}-nightly`.
- [ ] 2026-04-15 parity-audit: Git snapshot-mode short-circuit: `rev-parse HEAD` at `core/src/git.rs:245` bubbles when not in a repo; GoReleaser short-circuits earlier.
- [ ] 2026-04-15 parity-audit: Dead code at `cli/src/pipeline.rs:511-520` — unreachable warn-loop for unknown skip values (main.rs already errors). Delete.

**Announce**
- [ ] 2026-04-15 parity-audit: B45 Webhook default message_template uses generic bare-text; GoReleaser `webhook.go:21` uses `{"message":"..."}` JSON envelope which matches default Content-Type `application/json`. Add webhook-specific default. `stage-announce/src/lib.rs:28-29`.
- [ ] 2026-04-15 parity-audit: B4 Mattermost attachment only sent when color OR title set; GoReleaser always sends an attachment (with default title `{{ ProjectName }} {{ Tag }} is out!` and color `#2D313E`). `stage-announce/src/mattermost.rs:25`.
- [ ] 2026-04-15 parity-audit: B22 Snapcraft hooks/plugs/assumes emitted even when `apps` is empty; GoReleaser `snapcraft.go:338-402` drops them in that case. `stage-snapcraft/src/lib.rs:317-323`.
- [ ] 2026-04-15 parity-audit: B64 Bluesky PDS URL hardcoded to `https://bsky.social`; add optional `pds_url` config field for self-hosted servers. `stage-announce/src/bluesky.rs:4`.
- [ ] 2026-04-15 parity-audit: B44 Blob `include_meta` artifact type list differs from GoReleaser's `ReleaseUploadableTypes()`. Missing: Makeself, PyWheel, PySdist, UploadableFile. Extra: Snap, DiskImage, Installer, MacOsPackage. `stage-blob/src/lib.rs:501-536`. Extract shared helper `release_uploadable_kinds()` in core/artifact.rs.

**Packaging (nfpm / makeself / snapcraft)**
- [ ] 2026-04-15 parity-audit: B14 `ConventionalFileName` template variable is a hand-rolled string; should match nfpm's per-format logic (deb/rpm/apk/archlinux/ipk arch translation + separator differences). `stage-nfpm/src/lib.rs:1256-1259`.
- [ ] 2026-04-15 parity-audit: B16/O2 nfpm `mtime` config read but never applied to output file. `stage-nfpm/src/lib.rs` (missing `set_file_mtime` after `log.check_output(output, "nfpm")?`). GoReleaser `nfpm.go:577-581`.
- [ ] 2026-04-15 parity-audit: B15 nFPM Deb `arch_variant` missing — Goamd64=v2/v3 variant not passed. Add to `NfpmDebConfig` + `NfpmYamlDeb` + wiring.
- [ ] 2026-04-15 parity-audit: B18 nfpm `package_name` default is `krate.name`; GoReleaser uses ProjectName. `stage-nfpm/src/lib.rs:1243`.
- [ ] 2026-04-15 parity-audit: B11 nfpm termux.deb bindir/libdirs only prefix-rewrite paths starting with `/usr` or `/etc`; GoReleaser rewrites always. `stage-nfpm/src/lib.rs:430-454`.
- [ ] 2026-04-15 parity-audit: B27 Makeself default filename template missing Arm/Mips/Amd64 variant suffixes (ARM builds collide by name). `stage-makeself/src/lib.rs:255`.
- [ ] 2026-04-15 parity-audit: C4/C9 nfpm + snapcraft `builds:` → `ids:` migration: add `#[serde(alias = "builds")]` plus deprecation warning on `NfpmConfig.ids` / `SnapcraftConfig.ids`. `core/src/config.rs:2850,3181`.

**Parallelism**
- [ ] 2026-04-15 parity-audit: nfpm / snapcraft / makeself / flatpak packaging loops serial; GoReleaser uses `semerrgroup.New(ctx.Parallelism)`. Material slowdown for multi-platform/multi-crate builds.
- [ ] 2026-04-15 parity-audit: Blob — parallel per-file but serial across blob configs. GoReleaser parallelises configs too.

**Intentional (prior decisions, documented)**
- Teams uses AdaptiveCard not MessageCard (Session O documented).
- Blob KMS via CLI shell-out not gocloud.dev (Session O documented).
- UPX uses `targets` glob not goos/goarch (Rust target triples more precise).
- SRPM uses rpmbuild subprocess not nfpm Go library.
- Universal binary via lipo subprocess (macOS only).
- Build command uses explicit `--bin <name>`.
- `filter`/`reverseFilter` regex uses Rust regex vs POSIX ERE.

## Resolved

### 2026-04-15
- [x] 2026-04-15 parity-audit: B-5 binary_signs default template had stray `.sig` suffix diverging from GoReleaser `sign_binary.go:16`. Fixed: removed suffix; tests updated. `stage-sign/src/lib.rs:24`.
- [x] 2026-04-15 parity-audit: C5 Chocolatey template renders missing `Changelog` variable (GoReleaser `WithExtraFields`). Fixed: inject `Changelog` from `ReleaseNotes` in `stage-publish/src/chocolatey.rs:457-475`.
- [x] 2026-04-15 parity-audit: W7 Winget template renders missing `Changelog` variable. Fixed: same pattern applied in `stage-publish/src/winget.rs:779-792`.
- [x] 2026-04-15 parity-audit: W8 Winget `release_date` not coerced to YYYY-MM-DD. Fixed: slice first 10 chars of RFC-3339 Date; format-validated. `stage-publish/src/winget.rs:776`.
- [x] 2026-04-15 parity-audit: S4 Scoop bin paths used `\` separator; GoReleaser `scoop.go:384` uses `filepath.ToSlash`. Fixed: forward-slash. `stage-publish/src/scoop.rs:117`.
- [x] 2026-04-15 parity-audit: D5 Archive default ID missing — `archive.id` was Optional, downstream `ids:` filters couldn't match. Fixed: default to `"default"` when unset; always emit in metadata. `stage-archive/src/lib.rs:1471-1481`.
- [x] 2026-04-15 parity-audit: `MonorepoConfig` / `WorkspaceConfig` lacked `deny_unknown_fields` (typos silently accepted). Fixed: added to both structs. `core/src/config.rs:5108,5175`.

### Follow-up not addressed this session (false positives — verified against code)
- `Now.Format` — works (preprocessor converts to `Now | now_format(format="FMT")`).
- `github_urls.skip_tls_verify` — fully wired.
- `ANODIZE_CURRENT_TAG` + HEAD validation — matches GoReleaser (validate still runs).
- `--skip=unknown` — already errors in main.rs (dead warn-loop remains in pipeline.rs).
- AUR arch `arm7` — dead code would duplicate existing coverage.
- H2 Homebrew Goarm "6" — matches GoReleaser `experimental.DefaultGOARM`.
- U4 HTTP upload retry — GoReleaser does NOT retry artifactory/fury/cloudsmith/upload either.
- B63 Mastodon form-encoded POST — matches go-mastodon library (form-encoded is canonical).
- B6 Archive ids filter — matches GoReleaser (archive pipe only filters build types).
- B8 Artifact paths absolute — matches GoReleaser (no relative path normalization).
