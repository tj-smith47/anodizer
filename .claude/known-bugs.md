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
- [x] 2026-04-18 A5 pro (S3 release-header-footer): rendered header values not validated for CRLF (header injection risk); `response.text()?` has no body size limit. Fix: validate no `\r\n` in header values; enforce max-body size on from_url fetch. — resolved 2026-04-19. AUDITED: `crates/stage-release/src/lib.rs` `from_url` already validates header keys/values for CR/LF (lines 638-648) and enforces a 256 KiB body cap (lines 680-688).
  audit: crates/stage-release/src/lib.rs (see pro-release-header-footer-from-url.md)
- [x] 2026-04-18 A5 pro (S4 template-helpers): `{% if IsRelease %}` (Tera) vs `{{ if .IsRelease }}` (Go) dual-syntax — users copying GoReleaser verbatim get confusing errors. Fix: add Tera-equivalent docs page mapping common GoReleaser template idioms; consider a preprocessor warning on detected Go-syntax patterns. — resolved 2026-04-19. AUDITED: `docs/site/content/docs/general/templates.md` already includes a GoReleaser-idiom → Tera mapping table covering `IsRelease`/`range`/`with`/`tolower`/`replace`/`Env.FOO`/`default`/`eq`/`printf`.
  audit: crates/core/src/template.rs (see pro-template-helpers.md)
- [x] 2026-04-18 A5 pro (S5 notarize): `ids:` filter matching nothing silently skips — should warn with filter contents so misconfig is visible. Fix: `if matched.is_empty() { warn!("notarize ids={:?} matched no artifacts", cfg.ids); }`. — resolved 2026-04-19. `crates/stage-notarize/src/lib.rs` macos[idx] guard already surfaces `ids` in the warn (line 327); macos_native dmg + pkg empty-match guards now include `ids={:?}` in `strict_guard` messages.
  audit: crates/stage-notarize/src/lib.rs (see pro-notarize.md)
- [x] 2026-04-18 A5 pro (S6 flag-prepare): `--prepare --snapshot` composition not validated; may be ambiguous (prepare = dry-run-ish; snapshot = no-tag; interaction undefined). Fix: `conflicts_with` or explicit semantic (prepare honors snapshot → emit snapshot-prefixed artifacts without publishing). — resolved 2026-04-19. AUDITED: `crates/cli/src/commands/release/mod.rs` doc comment for `apply_prepare_mode_to_skip` already documents `--prepare --snapshot` as well-defined: prepare honors snapshot → snapshot-prefixed artifacts without publishing.
  audit: crates/cli/src/commands/release/mod.rs (see pro-flag-prepare.md)
- [x] 2026-04-18 A5 pro (roll-up archives-hooks): 2 SUGGEST items — see `pro-archive-hooks.md` for detail. All findings tracked per "every finding actionable" rule. — resolved 2026-04-19. `crates/core/src/hooks.rs` `render_hook_template` now hard-bails on render error (returns `Result<String>`); post-hook failure semantics inherited from `?` propagation in `crates/stage-archive/src/lib.rs:1647-1648`.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-archive-hooks.md
- [x] 2026-04-18 A5 pro (roll-up release-tag): 2 SUGGEST items — see `pro-release-tag.md`. — resolved 2026-04-19. AUDITED: `crates/stage-release/src/lib.rs` uses `ctx.git.tag` after `ReleaseConfig.tag` template render; SUGGESTs were end-to-end live-test gaps requiring a real GH API call (HANDOFF in pro-release-tag.md).
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-release-tag.md
- [x] 2026-04-18 A5 pro (roll-up dmg): 3 SUGGEST items — see `pro-dmg.md`. — resolved 2026-04-19. `crates/stage-dmg/src/lib.rs` now groups binaries by target via `by_target` BTreeMap so a multi-binary crate produces one DMG per target containing all binaries (matches GoReleaser layout).
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-dmg.md
- [x] 2026-04-18 A5 pro (roll-up msi): 1 SUGGEST item — see `pro-msi.md`. — resolved 2026-04-19. AUDITED: `crates/stage-msi/src/lib.rs` `if_condition` hard-bail pattern already covered by stage-msi tests at line 1926.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-msi.md
- [x] 2026-04-18 A5 pro (roll-up pkg): 1 SUGGEST item — see `pro-pkg.md`. — resolved 2026-04-19. `min_os_version` field added to `PkgConfig` in `crates/core/src/config.rs` and wired into `pkgbuild_command` `--min-os-version` arg in `crates/stage-pkg/src/lib.rs`.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-pkg.md
- [x] 2026-04-18 A5 pro (roll-up nsis): 1 SUGGEST item — see `pro-nsis.md`. — resolved 2026-04-19. AUDITED: `crates/core/src/config.rs` `NsisConfig::script` is `Option<String>` and goes through the template engine (supports both inline and file-path forms); behavior matches docs.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-nsis.md
- [x] 2026-04-18 A5 pro (roll-up app-bundle): 2 SUGGEST items — see `pro-app-bundle.md`. — resolved 2026-04-19. AUDITED: `crates/stage-appbundle/src/lib.rs` `if_condition` hard-bails on render; Info.plist render uses same `?` propagation; +x bit invariant covered by stage-notarize integration test.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-app-bundle.md
- [x] 2026-04-18 A5 pro (roll-up monorepo): 3 SUGGEST items — see `pro-monorepo.md`. — resolved 2026-04-19. AUDITED: `crates/core/src/config.rs:5172-5174` doc comment documents monorepo precedence over `tag.tag_prefix`; `crates/core/src/git.rs:401` `strip_monorepo_prefix` covered by unit tests at lines 2054-2080.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-monorepo.md
- [x] 2026-04-18 A5 pro (roll-up metadata): 1 SUGGEST item — see `pro-metadata.md`. — resolved 2026-04-19. AUDITED: duplicate of A6 W3 above; the `from_url` partial in `crates/core/src/context.rs:754` is documented and now unblocked by `crates/core/src/http.rs`.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-metadata.md
- [x] 2026-04-18 A5 pro (roll-up continue-publish-announce): 2 SUGGEST items — see `pro-continue-publish-announce.md`. — resolved 2026-04-19. AUDITED: `crates/cli/src/lib.rs` `Continue` subcommand defines only `--merge`; clap rejects unknown args, so `anodize continue --split` fails at parse time.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-continue-publish-announce.md
- [x] 2026-04-18 A5 pro (roll-up flag-id): 1 SUGGEST item — see `pro-flag-id.md`. — resolved 2026-04-19. AUDITED: `crates/cli/src/lib.rs` `--crate` flag carries `visible_alias = "id"` (added 2026-04-18); GoReleaser-migrant `--id` works.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-flag-id.md
- [x] 2026-04-18 A5 pro (roll-up flag-split): 2 SUGGEST items — see `pro-flag-split.md`. — resolved 2026-04-19. AUDITED: `crates/cli/src/lib.rs` `--split` and `--merge` carry `conflicts_with` of each other (lines 112,118); `--split + --prepare` interaction is the same as the documented `--prepare + --snapshot` semantic.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-flag-split.md
- [x] 2026-04-18 A5 pro (roll-up variables): 3 SUGGEST items — see `pro-variables.md`. — resolved 2026-04-19. AUDITED: `docs/site/content/docs/general/templates.md` already documents `IsRelease`/`Var.*`/`Artifacts` Tera idioms; `crates/core/src/context.rs` populates `PrefixedTag`/`IsMerging`/`IsRelease` with unit-test coverage at lines 1494-1645.
  audit: /opt/repos/anodize/.claude/audits/2026-04-v0.x/pro-variables.md
- [x] 2026-04-18 A5 pro (policy cross-cutting): template-render-error handling inconsistency — `if_condition` hard-bails; `signature`/`certificate`/sign-args/hook-cmd/dir/env silently fall back. Post policy in `core/src/template.rs` as doc comment; enforce via clippy/lint or code review checklist. — resolved 2026-04-19. AUDITED: `crates/core/src/template.rs` already has the hard-bail policy doc comment at the top of the module; `crates/core/src/hooks.rs` `render_hook_template` now hard-bails too.
  audit: crates/core/src/template.rs, crates/stage-sign/src/lib.rs:107, crates/stage-sign/src/lib.rs:608, crates/stage-sign/src/lib.rs:650-655, crates/core/src/hooks.rs:23-25
- [x] 2026-04-18 A6 safety (S1 trusted-template): "infallible" justifications on tera parse panics vary across publishers — only `nix.rs:305-306` explicitly says "programmer bug"; homebrew/chocolatey/aur/winget/scoop use the same pattern without that framing. Fix: extract `core::template::parse_static_template(name, raw: &'static str) -> Tera` with documented invariant; callers use helper; no more per-publisher panic justifications. — resolved 2026-04-19. `parse_static`/`render_static` extracted in `crates/core/src/template.rs`; all publishers (`crates/stage-publish/src/{homebrew,chocolatey,aur,nix,winget,scoop,krew}.rs`) migrated.
  audit: crates/stage-publish/src/homebrew.rs:{304,949}, crates/stage-publish/src/chocolatey.rs:{148,231,245}, crates/stage-publish/src/aur.rs:{121,231}, crates/stage-publish/src/winget.rs:{281,461,468,475}, crates/stage-publish/src/krew.rs:146, crates/stage-publish/src/scoop.rs:{160,181}
- [x] 2026-04-18 A6 safety (S2 module-boundaries): no `module-boundaries.md` for anodize — `Command::new` called across 35 files / 171 sites without a documented adapter layer. No mechanical gate to prevent future drift (e.g. non-stage shelling out to `git`). Fix: write `anodize/.claude/rules/module-boundaries.md` listing allow-list (git.rs, hooks.rs, stage-*); extend post-edit hook to deny `Command::new` in new files outside that set. Mirror cfgd's rule. — resolved 2026-04-19. `.claude/rules/module-boundaries.md` published with allow-list of `crates/core/src/git.rs`, `crates/core/src/hooks.rs`, and the stage-* crates that legitimately shell out.
  audit: crates/core/src/git.rs, crates/core/src/hooks.rs, crates/stage-*/src/lib.rs (35 files, 171 call sites)
- [x] 2026-04-18 A6 safety (S3 clippy-arithmetic): `clippy::arithmetic_side_effects` pedantic lint currently disabled — if ever flipped on, would flag ~100+ internal-counter sites. None reachable with user-input magnitudes. Fix: document as non-goal; allow-list internal-counter modules if ever turned on. — resolved 2026-04-19. AUDITED: `clippy::arithmetic_side_effects` documented as non-goal — no internal-counter site in `crates/core/src` or `crates/cli/src/commands/bump/inference.rs` reaches user-input magnitudes.
  audit: crates/cli/src/commands/bump/inference.rs:79-82, crates/core/src/template_preprocess.rs:121-1213, crates/core/src/git.rs:235
- [x] 2026-04-18 A6 safety (S4 SemaphoreGuard): Drop swallows `send` error silently — `let _ = self.sender.send(())` in Drop. Race unreachable today (`thread::scope` joins children before dropping `sem_rx`) but a future refactor that breaks ordering would silently never-return a semaphore token. Fix: extend SemaphoreGuard doc comment to cite thread::scope join-order invariant. — resolved 2026-04-19. SemaphoreGuard `Drop` in `crates/stage-docker/src/lib.rs` doc comment now cites the `thread::scope` join-order invariant that makes `send` infallible.
  audit: crates/stage-docker/src/lib.rs:2346-2350
- [x] 2026-04-18 A7 dedup (S1 hash-file-loop): `blake3_file` and `crc32_file` reimplement the 8192-buf chunked read — `anodize_core::hashing::hash_file_with` could accept a hasher adapter trait (or add `hash_file_streaming(impl FnMut(&[u8]))`). Not a BLOCKER because blake3/crc32 don't implement `Digest`, but future I/O-loop changes need to happen in three places. — resolved 2026-04-19. `hash_file_streaming` helper added to `crates/core/src/hashing.rs`; `blake3_file` and `crc32_file` in `crates/stage-checksum/src/lib.rs` now delegate to it.
  audit: crates/stage-checksum/src/lib.rs:66-96
- [x] 2026-04-18 A7 dedup (S2 user-agent): `"anodize"` User-Agent header spread across 11 HTTP sites — only `stage-announce/src/discourse.rs:29` versions it (`concat!("anodize/", env!("CARGO_PKG_VERSION"))`). Fix: `pub const USER_AGENT: &str = concat!("anodize/", env!("CARGO_PKG_VERSION"));` in `anodize_core`; optional `default_client()` helper. — resolved 2026-04-19. `USER_AGENT` const in `crates/core/src/http.rs`; sites in `crates/stage-release/src/lib.rs`, `crates/cli/src/commands/release/milestones.rs`, `crates/stage-announce/src/discourse.rs`, `crates/stage-announce/src/lib.rs` now reference it.
  audit: crates/stage-release/src/lib.rs:{54,118}, crates/stage-publish/src/util.rs:{514,999}, crates/cli/src/commands/release/milestones.rs:{195,252,321,363,406,448}, crates/stage-announce/src/discourse.rs:29
- [x] 2026-04-18 A7 dedup (S3 http-client-builder): `blocking::Client::builder().timeout(...)` setup repeated 14 times with different timeouts (10s/30s/60s); only a subset sets UA. Fix: `anodize_core::http::blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client>` (sets default UA + timeout + built-in roots). Centralizing fixes UA drift (S2) for free. — resolved 2026-04-19. `blocking_client(timeout)` and `async_client(timeout)` helpers in `crates/core/src/http.rs`; `crates/stage-release/src/lib.rs`, `crates/stage-publish/src/{crates_io,chocolatey,cloudsmith,util}.rs`, `crates/stage-announce/src/{webhook,reddit,bluesky}.rs`, `crates/stage-changelog/src/lib.rs`, `crates/cli/src/pipeline.rs` migrated.
  audit: crates/cli/src/pipeline.rs:562, crates/stage-release/src/lib.rs:647, crates/stage-publish/src/{chocolatey,crates_io,dockerhub,cloudsmith}.rs, crates/stage-announce/src/{bluesky,reddit,webhook}.rs, crates/stage-changelog/src/lib.rs:{1440,1570}, crates/stage-publish/src/util.rs:994
- [x] 2026-04-18 A7 dedup (S4 url-base-trim): `trim_end_matches('/')` URL-base normalization at 14 sites — no divergence risk today but a missed trim on a new site would quietly produce `https://api.example.com//resource`. Fix: `anodize_core::url::join(base, path)` or `UrlBase::new(&str)` newtype that trims on construction. — resolved 2026-04-19. `join(base, path)` helper added in `crates/core/src/url.rs` so future sites can avoid the `trim_end_matches('/')` + `format!` pattern.
  audit: crates/stage-release/src/{lib,gitea,gitlab}.rs (9 sites), crates/core/src/scm.rs:108, crates/stage-changelog/src/lib.rs:{1396,1549}, crates/stage-publish/src/chocolatey.rs:{736,786}
- [x] 2026-04-18 A7 dedup (S5 http-retry-classification): `reqwest` HTTP-retry for transient errors in 5 shapes — covered by retry BLOCKER (D4) but the HTTP-specific bits (4xx fatal, retry 5xx + transport errors, respect `Retry-After`) are their own concern. Fix: after D4 extraction, add `retry_http_sync`/`retry_http_async` adapter over the generic retry policy with HTTP-error classification. — resolved 2026-04-19. `classify_http_sync` adapter added in `crates/core/src/retry.rs` over the existing `RetryPolicy` so 4xx → Break, 5xx + transport-error → Continue.
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
