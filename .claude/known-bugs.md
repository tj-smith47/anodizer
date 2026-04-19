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

### cfgd v0.3.5 dogfooding — 2026-04-19 (3 BLOCKER)

Surfaced by reviewing the open winget + krew PRs anodize created for cfgd v0.3.5 (microsoft/winget-pkgs#361032, kubernetes-sigs/krew-index#5595). Both PRs currently fail CI because of generator bugs in anodize publishers. Fixes MUST land before cfgd can bump `ANODIZE_REV` and re-run the v0.3.5 pipeline (tags/releases already deleted on the cfgd side).

- [ ] 2026-04-19 dogfood (BLOCKER C1): publishers emit `sha256: ''` for every artifact because stage-checksum writes the hash as `metadata["Checksum"] = "algorithm:hash"` (see crates/stage-checksum/src/lib.rs:547-549) but every publisher reads `metadata.get("sha256").cloned().unwrap_or_default()`. The keys don't match. Only stage-notarize (crates/stage-notarize/src/lib.rs:39) writes the lowercase `sha256` key — notarized macOS artifacts are the only case that produces a non-empty hash today. Affected sites (8): crates/stage-publish/src/winget.rs:696, winget.rs:736, krew.rs (via util.rs:1116 in artifact_to_os_artifact), scoop.rs:318, homebrew.rs:453, homebrew.rs:502, homebrew.rs:1513, chocolatey.rs:397. Fix: in ChecksumStage::run, after building `checksum_map`, also write a lowercase `metadata.insert("sha256", hash)` (or `metadata.insert(algorithm.to_string(), hash)`) alongside the existing `"Checksum"` key. Alternatively switch every publisher call site to read `metadata["Checksum"]` and parse the `algorithm:` prefix — the former is one-site, safer. Symptoms in cfgd v0.3.5 PRs: winget manifest validator rejects with `Value type not permitted by 'type' constraint ... InstallerSha256` + `Required field missing. [InstallerSha256]`; krew CI fails with the same root cause.
- [ ] 2026-04-19 dogfood (BLOCKER C2): `map_target("darwin-universal")` returns arch `"darwin"` instead of something sensible like `"universal"` or `"all"`. Universal binaries are registered with `target: "darwin-universal"` in crates/stage-build/src/lib.rs:567; the map_target function in crates/core/src/target.rs:37-56 has no branch matching that arch, so the first-component fallback returns the literal string `"darwin"`. Downstream effects: krew manifest gets a third darwin entry with `os: darwin, arch: darwin`, archive naming produces `cfgd-0.3.5-darwin-darwin.tar.gz`, and krew CI validation (`kubernetes-sigs/krew-index#5595`) flags the malformed platform selector. Fix: extend `map_target` to match `darwin-universal` → arch `"all"` (or `"universal"` if a dedicated label is preferred), and verify crates/stage-publish/src/krew.rs:188-198 (already handles arch=="all" by expanding into amd64+arm64 entries) matches whatever label is chosen. Archive naming should also produce `cfgd-0.3.5-darwin-universal.tar.gz` or `cfgd-0.3.5-darwin-all.tar.gz`, not `darwin-darwin`.
- [ ] 2026-04-19 dogfood (BLOCKER C3): default commit author `anodize <bot@anodize.dev>` (crates/stage-publish/src/util.rs:681) blocks EasyCLA checks on any CNCF-project publisher PR. `bot@anodize.dev` is not a GitHub-registered email, so the Linux Foundation EasyCLA bot cannot match the commit author to a signed CLA. Today's cfgd v0.3.5 krew PR (kubernetes-sigs/krew-index#5595) is stuck on this. Fix options, best-to-worst: (1) make the default resolution order `commit_author.{name,email} → git config user.{name,email} → bot@anodize.dev` so a correctly-configured machine produces commits from the release-engineer's identity; (2) require users to override via `commit_author:` in their anodize config (current escape hatch — works, but catches every new consumer who hasn't read the docs); (3) register `bot@anodize.dev` against a GitHub App and sign the CNCF EasyCLA once as a shared identity (most work, permanent for every anodize user).

### Wave A consolidation — 2026-04-18 (cycle 2)

Findings from `/opt/repos/anodize/.claude/audits/2026-04-v0.x/` audit files. Totals: **10 BLOCKER / 21 WARN / 57 SUGGEST**. `findings_skipped_dup=0` (Active was empty when this consolidation started; no signature collisions possible). Every row carries an `audit:` file:line reference per anodize convention. Sources: A2 parity-build-archive, A3 parity-publishers, A4 parity-announcers, A5 pro-features-audit (+21 per-feature files), A6 safety, A7 dedup.

#### Blockers

  audit: crates/stage-nfpm/src/lib.rs:636-637 (goreleaser: internal/pipe/nfpm/nfpm.go:636-637)
  audit: crates/stage-snapcraft/src/lib.rs:627-629 (goreleaser: internal/pipe/snapcraft/snapcraft.go:103,120-122)
  audit: crates/stage-blob/src/lib.rs:406-419 (goreleaser: internal/pipe/blob/upload.go:268-291)
  audit: crates/stage-release/src/lib.rs:1085-1098 (goreleaser: internal/pipe/release/body.go:24-44)
  audit: crates/stage-build/src/lib.rs:805, crates/cli/src/pipeline.rs:476, crates/stage-sign/src/lib.rs:163
  audit: crates/core/src/util.rs:203, crates/stage-build/src/lib.rs:1098
  audit: crates/cli/src/pipeline.rs:1558-1826, crates/stage-build/src/lib.rs:3856-3888
  audit: crates/stage-docker/src/lib.rs:{811-944,964-1074,2658-2800}, crates/stage-release/src/lib.rs:{345-373,651-697,2233-2340}
  audit: crates/stage-release/src/{gitea.rs:24,gitlab.rs:{26,30},lib.rs:23}
  audit: crates/cli/src/commands/helpers.rs:394-406, crates/cli/src/commands/helpers.rs:700-710

#### Warnings

  audit: crates/stage-archive/src/lib.rs:304-325, crates/stage-archive/src/lib.rs:1525, crates/stage-archive/src/lib.rs:1600-1608 (goreleaser: internal/pipe/archive/archive.go:143-145,296-336)
  audit: crates/core/src/config.rs:1168, crates/stage-archive/src/lib.rs:1071, crates/stage-archive/src/lib.rs:1094-1105
  audit: crates/stage-build/src/lib.rs:1742-1754 (goreleaser: internal/builders/base/build.go:91-105)
  audit: crates/stage-docker/src/lib.rs:96-120 (goreleaser: internal/pipe/docker/docker.go:389-418)
  audit: crates/stage-docker/src/lib.rs:692-694 (goreleaser: internal/pipe/docker/docker.go:343,346)
  audit: crates/stage-docker/src/lib.rs:367-376 (goreleaser: internal/pipe/docker/docker_test.go:1501)
  audit: crates/stage-publish/src/artifactory.rs:393-397 (goreleaser: internal/http/http.go:168-178)
  audit: crates/stage-publish/src/upload.rs:54-56 (goreleaser: internal/http/http.go:163-164,176-177)
  audit: crates/stage-nfpm/src/lib.rs:638, crates/stage-nfpm/src/lib.rs:646, crates/stage-nfpm/src/lib.rs:652 (goreleaser: internal/pipe/nfpm/nfpm.go:640)
  audit: crates/core/src/config.rs:4758-4760, crates/stage-announce/src/lib.rs:782-807 (goreleaser: internal/pipe/smtp/smtp.go:98-121)
  audit: crates/stage-dmg/src/lib.rs (see pro-dmg.md for exact line)
  audit: crates/stage-msi/src/lib.rs (see pro-msi.md for exact line)
  audit: crates/core/src/partial.rs, crates/cli/src/commands/release/split.rs (see pro-partial-builds.md)
  audit: crates/core/src/partial.rs (see pro-partial-builds.md)
  audit: crates/cli/src/commands/release/mod.rs (see pro-flag-id.md)
  audit: crates/stage-publish/src/homebrew.rs:389, crates/stage-publish/src/homebrew.rs:1220, crates/stage-publish/src/chocolatey.rs:206, crates/stage-publish/src/chocolatey.rs:238, crates/stage-publish/src/chocolatey.rs:254, crates/stage-publish/src/aur.rs:200, crates/stage-publish/src/aur.rs:279
  audit: crates/cli/src/commands/helpers.rs:421, crates/cli/src/commands/helpers.rs:424
  audit: crates/core/src/context.rs:754
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/safety.md
  audit: crates/core/src/context.rs:754 (already tracked: above W3)

#### Suggestions

  audit: crates/stage-source/src/lib.rs:65 (goreleaser: internal/pipe/sourcearchive/source.go:54-57)
  audit: crates/cli/src/commands/helpers.rs:136, crates/cli/src/commands/helpers.rs:239 (goreleaser: internal/pipe/git/git.go:268,292)
  audit: crates/core/src/config.rs:507, crates/core/src/config.rs:514, crates/core/src/config.rs:521 (goreleaser: internal/pipe/env/env.go:42-53)
  audit: crates/core/src/config.rs:1192-1201, crates/stage-archive/src/lib.rs:773-787 (goreleaser: internal/pipe/archive/archive.go:338-354)
  audit: crates/stage-build/src/lib.rs:2017-2027 (goreleaser: internal/pipe/build/build.go:147-155)
  audit: crates/core/src/config.rs:975-984 (goreleaser: customization/archive — before/after docs-only)
  audit: crates/stage-publish/src/util.rs:670-676 (goreleaser: internal/commitauthor/author.go:11-13)
  audit: crates/stage-publish/src/util.rs:867-884 (goreleaser: internal/commitauthor/author.go:49-52)
  audit: crates/stage-publish/src/aur.rs:385-398 (goreleaser: internal/pipe/aur/aur.go:58-63)
  audit: crates/stage-publish/src/homebrew.rs:786 (goreleaser: internal/pipe/brew/brew.go:77)
  audit: crates/stage-snapcraft/src/lib.rs (goreleaser: internal/pipe/snapcraft/snapcraft.go:123-126)
  audit: crates/stage-blob/src/lib.rs:319-328 (goreleaser: internal/pipe/blob/upload.go:113-119)
  audit: crates/cli/src/commands/release/milestones.rs:41 (goreleaser: internal/pipe/milestone/milestone.go:30-41)
  audit: crates/stage-docker/src/lib.rs:2609 (goreleaser: internal/pipe/docker/manifest.go)
  audit: crates/stage-publish/src/homebrew.rs:1242-1259 (goreleaser: internal/pipe/brew/brew.go:143-149)
  audit: crates/stage-docker/src/lib.rs:129-133 (goreleaser: internal/pipe/docker/v2/docker.go:544-549)
  audit: crates/stage-release/src/lib.rs:1484-1500, crates/stage-release/src/lib.rs:1688-1694 (goreleaser: internal/pipe/release/release.go:41-53)
  audit: crates/stage-announce/src/lib.rs:317-337 (goreleaser: internal/pipe/webhook/webhook.go:104-115)
  audit: crates/stage-announce/src/lib.rs:501-502 (goreleaser: internal/pipe/mattermost/mattermost.go:48-49,82)
  audit: crates/stage-sign/src/lib.rs:107, crates/stage-sign/src/lib.rs:608 (see pro-signs-if.md)
  audit: crates/core/src/partial.rs (see pro-partial-builds.md)
  audit: crates/stage-release/src/lib.rs (see pro-release-header-footer-from-url.md)
  audit: crates/core/src/template.rs (see pro-template-helpers.md)
  audit: crates/stage-notarize/src/lib.rs (see pro-notarize.md)
  audit: crates/cli/src/commands/release/mod.rs (see pro-flag-prepare.md)
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-archive-hooks.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-release-tag.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-dmg.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-msi.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-pkg.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-nsis.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-app-bundle.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-monorepo.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-metadata.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-continue-publish-announce.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-flag-id.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-flag-split.md
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-variables.md
  audit: crates/core/src/template.rs, crates/stage-sign/src/lib.rs:107, crates/stage-sign/src/lib.rs:608, crates/stage-sign/src/lib.rs:650-655, crates/core/src/hooks.rs:23-25
  audit: crates/stage-publish/src/homebrew.rs:{304,949}, crates/stage-publish/src/chocolatey.rs:{148,231,245}, crates/stage-publish/src/aur.rs:{121,231}, crates/stage-publish/src/winget.rs:{281,461,468,475}, crates/stage-publish/src/krew.rs:146, crates/stage-publish/src/scoop.rs:{160,181}
  audit: crates/core/src/git.rs, crates/core/src/hooks.rs, crates/stage-*/src/lib.rs (35 files, 171 call sites)
  audit: crates/cli/src/commands/bump/inference.rs:79-82, crates/core/src/template_preprocess.rs:121-1213, crates/core/src/git.rs:235
  audit: crates/stage-docker/src/lib.rs:2346-2350
  audit: crates/stage-checksum/src/lib.rs:66-96
  audit: crates/stage-release/src/lib.rs:{54,118}, crates/stage-publish/src/util.rs:{514,999}, crates/cli/src/commands/release/milestones.rs:{195,252,321,363,406,448}, crates/stage-announce/src/discourse.rs:29
  audit: crates/cli/src/pipeline.rs:562, crates/stage-release/src/lib.rs:647, crates/stage-publish/src/{chocolatey,crates_io,dockerhub,cloudsmith}.rs, crates/stage-announce/src/{bluesky,reddit,webhook}.rs, crates/stage-changelog/src/lib.rs:{1440,1570}, crates/stage-publish/src/util.rs:994
  audit: crates/stage-release/src/{lib,gitea,gitlab}.rs (9 sites), crates/core/src/scm.rs:108, crates/stage-changelog/src/lib.rs:{1396,1549}, crates/stage-publish/src/chocolatey.rs:{736,786}
  audit: crates/stage-release/src/lib.rs:{345-373,651-697,2233-2340}, crates/stage-publish/src/util.rs:420-471, crates/stage-publish/src/crates_io.rs:120-194

## Design notes (non-task observations, do-not-re-audit)

Durable observations about how the tooling works, not fix-targets. Plain
bullets (not checkbox lines) so the push gate correctly skips them.

- **PostToolUse exit 2 is advisory, not preventive** — the write has already
  landed when the hook runs. It yells via stderr but cannot undo or prevent
  the next tool call. This is why rules-in-hooks have been skippable. The
  current design keeps blocking exits only for (a) secrets/tokens, (b)
  release gestures during active wave, (c) force-push/hard-reset. Everything
  else is advisory + known-bugs entry + Stop-hook's known-bugs push gate as
  the real enforcement. (2026-04-15 hook-audit.)

**Intentional parity divergences (prior decisions, documented)**
- Teams uses AdaptiveCard not MessageCard (Session O documented).
- Blob KMS via CLI shell-out not gocloud.dev (Session O documented).
- UPX uses `targets` glob not goos/goarch (Rust target triples are more precise).
- SRPM uses rpmbuild subprocess not nfpm Go library.
- Universal binary via lipo subprocess (macOS only).
- Build command uses explicit `--bin <name>`.
- `filter`/`reverseFilter` regex uses Rust regex vs POSIX ERE.

## Inventory pre-seeds (inheritance for parity auditor · do-not-re-audit)

These are durable decisions — not tasks. The `goreleaser-inventory-mapper` reads this section and writes matching rows into `anodize/.claude/specs/goreleaser-complete-feature-inventory.md` with `parity_status=implemented`, `notes` carrying the verification date + upstream ref, so future parity auditors read and skip. Bullet form (no `[ ]`) so the push gate doesn't treat these as unchecked tasks.

### Verified matching upstream (2026-04-15 — GoReleaser HEAD as of that date)
Citations to enrich during inventory mapping (A1). Flag as `needs-citation` in the inventory if upstream file:line cannot be pinned.
- `Now.Format` — implemented. Preprocessor rewrites `{{ .Now.Format "FMT" }}` to `{{ Now | now_format(format="FMT") }}`. Anodize ref: `core/src/template.rs` (search `now_format`).
- `github_urls.skip_tls_verify` — fully wired in the GitHub client.
- `ANODIZE_CURRENT_TAG` + HEAD validation — matches GoReleaser (`validate` still runs even when env tag is set).
- `--skip=unknown` — errors at parse time in `main.rs`; the warn-loop in `pipeline.rs:511-520` is dead. (Deletion of the dead loop is already an Active item — keep that; this line just affirms main.rs behaviour is correct.)
- AUR arch `arm7` — intentionally absent; would duplicate existing coverage.
- H2 Homebrew `Goarm = "6"` — matches GoReleaser `experimental.DefaultGOARM`.
- B63 Mastodon form-encoded POST — matches `go-mastodon` library (form-encoded is canonical).
- B6 Archive ids filter — matches GoReleaser (archive pipe filters build types only, not per-id).
- B8 Artifact paths absolute — matches GoReleaser (no relative path normalization).

### Rust-additive candidates promoted from false-positive review
These were filed as "GoReleaser doesn't do it either," but anodize claims superiority; these are opportunities, not bugs. Mapper records them as rust-additive rows.
- HTTP upload retry for artifactory / fury / cloudsmith / custom-upload publishers. GoReleaser does NOT retry; anodize can, using the same retry/backoff infrastructure Docker V2 uses. Surface as `rust-additive` candidate; decide in a follow-up scope pass whether to implement.

## Resolved

### Archived 2026-04-18


### 2026-04-15

See git history; ~25 fixes landed (GitLab JOB-TOKEN, GitHub draft-URL email-link bug, Sign artifacts:all alignment, Release `include_meta`, Milestone provider resolution, AUR master branch, Custom publisher default filter, Docker ID uniqueness V1+V2, Docker V2 retry scope, Docker legacy cardinality, `docker manifest rm` tolerance, SBOM binary-like dedup, Bluesky PDS URL, plus 10+ verified-correct false positives).

(Moved: `Follow-up not addressed` block relocated to **Inventory pre-seeds** section above, so the parity inventory mapper inherits the decisions and re-audits skip re-discovery.)
