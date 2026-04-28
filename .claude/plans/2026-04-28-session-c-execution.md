# Session C — execution plan

Date: 2026-04-28
Status: ready to execute. The 3 cross-cutting policies are committed; this plan covers the per-publisher behavior work (the heavy half of Session C) plus the lazy-defaults follow-on.

## Inputs

- Behavior-finding inventory: `.claude/audits/2026-04-config-gaps/_session-c-inputs.md` (28 (c) items + 3 cross-cutting policies).
- Schema gaps already settled in Session B (WAVE 1–6 + post-WAVE deferreds drained 2026-04-28).
- Cross-cutting policy precedents:
  - `3f8f2a8` brand-default policy (`DEFAULT_DISPLAY_NAME` const, no avatar URL)
  - `ff3be47` lazy-vs-eager defaults (`resolved_*()` accessors on config impls — precedent: `stage-checksum`)
  - `f6798a1` skip-when-empty UX (`Context::strict_guard` for missing-required — precedent: 6 announcers)

## Operating rules

1. **`task commit` per change** — never `git commit` directly (Taskfile gates fmt + clippy + tests + dry-run-release).
2. **No push without an explicit "push" instruction.**
3. **STOP for user approval per publisher** — Stream 2 cycles `current-vs-GR table → propose decisions → STOP → implement approved → review → next`. Heaviest task in the program; do not batch publishers without approval.
4. **No session-note comments in source** — rationale lives in commit messages and this plan, not in `// session C: ...` comments.
5. **Spec + code review per task** — both reviews mandatory; fix every finding regardless of severity label.

---

## Stream 1 — lazy-defaults follow-on (mechanical, no STOPs)

Apply the `resolved_*()` accessor pattern (precedent: `stage-checksum` / `ff3be47`) to the remaining 5 stages. Each commit is self-contained and reviewable in isolation.

| Order | Stage | Config struct | Defaults to lift |
|---|---|---|---|
| 1.1 | `stage-sign` | `SignConfig`, `DockerSignConfig` | `cmd` (cosign/gpg), `signature` template, `artifacts` filter |
| 1.2 | `stage-notarize` | `NotarizeConfig` + `MacOSNativeNotarizeConfig` | profile_name fallbacks, `wait` default, `timeout` default |
| 1.3 | `stage-sbom` | `SbomConfig` | `cmd` (`syft`), `documents` template, `args` defaults |
| 1.4 | `stage-release` | `ReleaseConfig` | `name_template`, `mode` default, `make_latest` resolution |
| 1.5 | `stage-changelog` + `crates/cli/src/commands/release/milestones.rs` | `ChangelogConfig`, `MilestoneConfig` | `format`, `sort`, milestone `name_template` |

Each commit shape:
- Add `DEFAULT_*` associated consts on the config struct(s) for every default string/template.
- Add `resolved_*()` inherent methods returning the resolved value.
- Migrate the stage's call sites from inline `.unwrap_or_else(|| "...")` to the accessor / const.
- Add regression tests (default + user-overrides) per accessor.
- xtask gen-docs runs as part of `task commit`.

Risk: low. Behavior-preserving except where the GR-canonical default literally differs from anodize's current default — those cases are flagged at commit time and either align or document the divergence in the const docstring.

Stream 1 unblocks Stream 2 because every Stream 2 publisher decision touches default values; pinning them behind named accessors first means the per-publisher discussions reason about a single source of truth instead of scattered literals.

---

## Stream 2 — per-publisher behavior decisions (heavy, STOP gates)

The 25 remaining (c) items grouped by publisher / stage. Smallest blast radius first; release/changelog last (largest surface, 9 items).

### Group A: milestone (3 items)

Source: `_session-c-inputs.md` C-new-20/21/22.

| ID | Decision |
|---|---|
| C-new-20 | Milestone `Repo` resolution: Default()-time vs publish-time |
| C-new-21 | Empty-after-render `name`: skip vs error (GR doesn't skip) |
| C-new-22 | Empty-repo + no-`fail_on_error`: anodize permissive vs GR strict |

Cycle: read `internal/pipe/milestone/milestone.go` → write current-vs-GR table for each → propose decisions → STOP → implement approved → review.

### Group B: checksum (1 item)

| ID | Decision |
|---|---|
| C-new-23 | Artifact-source kinds: anodize narrower than GR (missing Makeself/Flatpak/SourceRpm/Signature/Certificate/UploadableFile). Cross-link with `release_uploadable_kinds()` (audit 6 L4) so checksum + release-upload include the same set. |

Single-item group but cross-cutting — touches `stage-checksum` and `stage-release` together.

### Group C: sign / notarize / sbom (existing C3 agenda items)

From `parity-session-index.md` § C3 plus C-new-7:

| ID | Decision |
|---|---|
| C-new-7 | DockerV2 `SBOM` default: anodize off → flip to on (matches GR). |
| existing-C3 | nFPM `Libdirs` apply unconditionally — confirmed at `docker-nfpm-installers.md` item 6. |

These are smaller, well-defined deltas. Treat as a single STOP gate.

### Group D: build / archive / source (6 items)

C-new-1 through C-new-6. The build/archive surface is anodize's largest single source of GR-divergence findings.

| ID | Decision |
|---|---|
| C-new-1 | Universal-binary metadata-copy whitelist — define behavior contract |
| C-new-2 | `build.env.get(target)` exact vs glob match |
| C-new-3 | Universal-binary metadata both `id`+`binary` (GR id only) |
| C-new-4 | `archives: []` skip vs GR auto-inject |
| C-new-5 | `FormatOverride` exact `==` vs GR `HasPrefix` |
| C-new-6 | Default extra-file glob order (anodize LICENSE-first, GR license-first) |

Largest STOP gate by item count. Several items interact — e.g. C-new-1 and C-new-3 both touch universal-binary metadata. Surface the interactions in the current-vs-GR write-up so the user can decide them coherently.

### Group E: publishers — homebrew / chocolatey / AUR / crates_io (5 items)

| ID | Decision |
|---|---|
| C-new-9 | Homebrew `arm_variant` default `"6"` hardcode — verify or align with GR `experimental.DefaultGOARM`. |
| C-new-10 | TopLevelHomebrewCaskConfig `Directory="Casks"` (existing C3). |
| C-new-11 | Chocolatey idempotency port to crates_io. |
| C-new-12 | AUR `Name`/`Conflicts`/`Provides`/`Rel` defaults (existing C3). |
| C-new-13 | crates_io idempotency (port chocolatey hash-compare). |

Two of these (C-new-11 + C-new-13) are the same pattern in two stages — a single decision settles both.

### Group F: release / changelog (9 items)

C-new-14 through C-new-19. Largest group, deferred to last because changelog/release have the most user-visible surface and the most internal coupling.

| ID | Decision |
|---|---|
| C-new-14 | `prerelease == "auto"` Default()-time global vs per-tag run-time |
| C-new-15 | Snapshot mode runs changelog (GR skips) — flip to opt-in (already partially done via `ChangelogConfig.snapshot` field; this gate locks the *default*) |
| C-new-16 | Default `Format` SCM-mode `{{ ShortSHA }}` vs GR `.SHA` (full) |
| C-new-17 | `## Changelog` title escape-hatch (anodize skips on `title=""`, GR has no opt-out) |
| C-new-18 | Header/footer go to disk only — should `--release-header`/`--release-footer` reach release notes too (GR behavior) |
| C-new-19 | SCM changelogers always-API-call when no previous tag — pre-empt to git fallback like GR |

Several items are independent decisions that could land in their own commits; user may prefer to STOP per item rather than per group here.

### Group G: announcer tail-end (5 items)

The brand-default decision (`3f8f2a8`) settled the largest announcer cluster. These 5 are the residual:

| ID | Decision |
|---|---|
| C-new-25 | Skip-when-empty UX inconsistency — partially solved by `f6798a1` (6 announcers); follow-up: extend `strict_guard` to `require_rendered` (discourse, telegram, reddit, mastodon, bluesky, opencollective) |
| C-new-26 | Webhook User-Agent `anodizer/x.y.z` vs GR `goreleaser` — keep or align |
| C-new-27 | SMTP port default `587` vs GR `0` (errors) — keep permissive or align |
| C-new-28 | Mattermost channel/username/icon template-rendering inconsistency |

Lightweight group; can land as a single STOP gate with 4 small decisions.

---

## Per-publisher cycle template

For each Stream 2 group, the cycle is:

```
1. READ        Open the relevant GoReleaser pipe(s) at /opt/repos/goreleaser
               and the corresponding anodize stage. List every (c) item in
               the group.
2. TABLE       Write a current-vs-GR table per item: anodize file:line,
               GR file:line, observed default / behavior, intentional
               divergence reason if any.
3. PROPOSE     One recommendation per item with concrete YAML or Rust
               example showing the user-visible difference. Max 3 options
               per item; reserve discussion for real UX tradeoffs (memory:
               feedback_no_interrogation_pattern).
4. STOP        Surface the proposals to the user. Wait for approval.
5. IMPLEMENT   Code the approved decisions. One commit per item unless
               two items are mechanically inseparable.
6. REVIEW      Spec-review + code-review per implementation task. Fix
               every finding regardless of severity. Re-review until zero
               issues remain.
7. NEXT        Move to the next group.
```

## Exit criteria

- Stream 1: all 5 stages migrated (5 commits), accessors covered by regression tests.
- Stream 2: every (c) item in `_session-c-inputs.md` has either an implementation commit or a documented "intentional divergence" entry in `parity-session-index.md`.
- `cargo test --workspace` green at every commit; no skipped tests; no `task lint` warnings.
- After each group: known-bugs.md unchecked count remains 0 (any new findings raised during implementation get fixed before moving on).

## What this plan deliberately does NOT do

- Does NOT push (always wait for explicit "push" / "ship").
- Does NOT touch Session B schema items — those are settled.
- Does NOT add new publisher types or new stages.
- Does NOT include the v0.2.0 release cut, cfgd migration, or community adoption — those follow Session C.

## Resume protocol

When picking this back up:

1. Read this file top to bottom.
2. `git log --oneline | grep -E "Session C|stream 1|stream 2"` to see what's landed.
3. Identify the next un-completed item in Stream 1 (mechanical) or the next unlocked group in Stream 2 (per-publisher).
4. If Stream 2 group is unlocked, run the per-publisher cycle template above.
