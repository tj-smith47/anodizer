# 2026-04 config gaps — Session A index

Wrap-up index for Session A of the anodizer cohesive-refactor program
(`/root/.claude/plans/anodizer-refactor-program.md`). Session A landed
five GoReleaser parity audits, categorised the findings into four
buckets, and applied the in-session-actionable buckets (a) and (d).

## Audits in this set

| # | File | Scope |
|---|---|---|
| 1 | [build-archive-source.md](build-archive-source.md) | stage-build, stage-archive, stage-source |
| 2 | [docker-nfpm-installers.md](docker-nfpm-installers.md) | stage-docker, stage-nfpm, stage-msi/nsis/pkg/dmg/appbundle |
| 3 | [publishers-pkgmgr.md](publishers-pkgmgr.md) | homebrew, scoop, winget, krew, nix, aur, chocolatey, cratesio |
| 4 | [release-integrity_pass-a.md](release-integrity_pass-a.md) + [pass-b.md](release-integrity_pass-b.md) | stage-release, stage-changelog, milestones, checksum, sign, notarize, sbom |
| 5 | [infra-announcers_pass-a.md](infra-announcers_pass-a.md) + [pass-b.md](infra-announcers_pass-b.md) | dockerhub, artifactory, upload, cloudsmith, blob, all announcers |

Plus root-cause sub-audits:
- [_root-cause-chocolatey.md](_root-cause-chocolatey.md)
- [_root-cause-krew.md](_root-cause-krew.md)

## Categorization

[`_categorization.md`](_categorization.md) is the master per-finding
ledger. Totals:

| Bucket | Count | Owner |
|---|---|---|
| (a) production bug | 140 | Session A (in-session) |
| (b) config-schema | 33 | [_session-b-inputs.md](_session-b-inputs.md) |
| (c) publisher-behavior | 33 | [_session-c-inputs.md](_session-c-inputs.md) |
| (d) docs/comment | 15 | Session A (in-session) |
| done (already shipped) | 3 | — |
| verified-OK / no-op | 26 | — |

Grand total: **250 findings** across the five audit areas.

## Batches landed in Session A

The categorization recommended ten approval-batches; commits that landed
each are listed below for traceability.

| Batch | Theme | Commit(s) |
|---|---|---|
| 10 | stage-nfpm production `panic!` → `Result<>` | `9505686` |
| 1 | `eprintln!` → `StageLogger` (3 audit-flagged + 5 bonus) | `9505686` |
| 5 | 15 doc/comment fixes | `9505686` |
| 4 | Default()-time validation (~10 items): N4, N8, L15, AN39, AN42, K (validate_algorithm), A12 (case-insensitive upload mode) | `9505686`, `f43ce2f`, `4ddd456` |
| 3 | `name: String::new()` → derive from path.file_name() (4 sites) | `f43ce2f` |
| 2 | Template render-error swallows (~12 sites) | `13844da` |
| 6 | Cross-cutting cleanups: L4 (installers in release-uploadable), A1 / U2 (config-first password cascade), `try_is_disabled` for stage-notarize + stage-sbom | `513fc30`, `00f9957`, `2697cef` |
| 9 | Per-publisher production bugs — first wave: artifactory A2/A8/A10, upload U3/U9, dockerhub D2/D10 | `d9ce3a5` |
| 8 | Tokio runtime reuse — milestones M4 (3 sites collapsed to one runtime) | `27c58cf` |
| 7 | Stage monolith splits (3) | **not landed in Session A** — defer to dedicated splitting session (stage-archive 1700L, stage-release 5800L+, stage-sign 3700L; refactor-only, no behaviour change) |

Plus the `f43ce2f` archive default-extra-files glob bug found while
running Session A's tests (root-cause: `resolve_default_extra_files()`
globbed CWD instead of the crate dir, leaking the workspace's own
README into per-crate archives during `cargo test` runs).

## Items not yet landed in Session A

Most of the per-publisher (a) items remain. The first wave (Batch 9
above) covered the highest-impact authentication / image-validation
foot-guns; the second wave is mechanical and lands in follow-on
batches:

- **artifactory remaining**: A4, A5, A6 (JSON error parser
  `errors[].status`), A7 (1119-line module split), A9 (dry-run vs
  live render drift), A11 (PEM empty-after-parse hard-error vs GR
  soft-skip).
- **upload remaining**: U4–U8, U10–U12.
- **dockerhub remaining**: D3–D9 (notably D7 client pool, D8 secret
  validation before dry-run skip). D11 (from_url/from_file
  exactly-one) was landed in this set then user-reverted; do **not**
  re-apply.
- **stage-release**: R2 (owner/name template render), R4 (ReleaseURL
  variable), R5 (body template structure), R6 (Checksums map keys),
  R7, R9 (`skip_upload: "yes"` falls through), R13 (eprintln warning),
  R14 (5732-line split — Batch 7).
- **stage-changelog**: C5, C6, C9, C10, C13, C14, C15.
- **milestones**: M5 (URL strip-then-append), M8 (silent
  "milestone not found"), M9 (Gitea PATCH includes `title`), M10
  (double-iteration fallback). M4 done.
- **stage-checksum**: K1 (lazy default vs eager), K3 (kind label
  `Archive` vs `UploadableFile`), K5 (split + non-template overwrite),
  K6 (non-UTF8 filename), K7 (already done by Batch 2), K8 (extra-file
  per-crate in workspace runs).
- **stage-sign**: S4 (lazy default), S6/S8 (`docker_signs.artifacts`
  validation), S9, S10, S13, S14 (cache `default_sign_cmd`), S15, S16,
  S17 (3729-line split — Batch 7).
- **stage-notarize remaining**: N3 (hard-coded Apple timestamp URL),
  N5 (macos + macos_native both populated), N10, N11, N12, N14.
- **stage-announce remaining**: AN1–AN46 (~21 (a) items spanning
  all providers — discord rate-limit, slack token format, mastodon
  visibility validation, etc.).
- **`StringOrBool::is_disabled` legacy callers**: 30+ sites still
  call the silently-swallowing legacy method. Migrating each to
  `try_is_disabled` is mechanical and will land alongside each
  publisher's per-publisher batch above (rather than a single
  cross-cutting commit). The fallible `try_*` API is in place as of
  `00f9957`.
- **Shared HTTP-upload helper extraction** (cross-cutting, ~600L
  of duplication between `artifactory.rs` and `upload.rs`) lands
  with Batch 7's stage-publish split, since the helper crystallises
  the boundary between the two publishers.

## Handoff to Sessions B and C

- Session B (config-schema): consume [`_session-b-inputs.md`](_session-b-inputs.md).
- Session C (publisher-behavior): consume [`_session-c-inputs.md`](_session-c-inputs.md).

Both handoff files were authored together with the categorization in
the audit-landing pass and have not been edited since.

## Test posture

Workspace test count at session close: **~3000 unit/lib tests** across
27 crates — all green. clippy `--all-targets -- -D warnings` clean.
`task lint` (fmt + build + clippy + xtask gen-docs + dry-run release)
green for every commit in this session.
