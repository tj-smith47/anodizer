# Session B inventory — merged & deduped

Date: 2026-04-26
Supersedes: `_session-b-inputs.md` (kept for history; do not edit further).
Sources merged:
- `_categorization.md` — 33 (b)-bucket findings across 7 audits.
- `/root/.claude/plans/anodizer-refactor-program.md` § B1–B6 — structural items.
- This walk session — pre-walk design decisions DEC-1..6 below.

This file is the single working inventory for Session B. Every config-schema decision in flight is in here exactly once. Walk order is locked at the bottom (`## Walk plan`).

---

## Resolved pre-walk decisions

These are locked. They constrain every ITEM / SCH / FOLL below. No re-litigation in per-item walk.

| ID | Decision | Why |
|---|---|---|
| **DEC-1** | Publisher rename **`crates:` → `cargo:`** | Cohesion: every other publisher keys on the package-manager binary name (`brew`, `scoop`, `choco`, `winget`, `kubectl krew`, `nix`, `makepkg`). `cargo` extends that pattern; `cratesio` (registry-host) was the outlier. |
| **DEC-2** | Defaults **shadow** the config under the same dotted path. No parallel top-level publisher fan-out lists (B-new-16 dropped from this scope). | One inheritance axis, no two-ways-to-do-the-same-thing. `defaults.<path>` is the single source of inheritance. |
| **DEC-3** | At per-crate level, multi-publisher fields accept **list OR scalar** (untagged-enum coercion); at defaults level, **always single struct**. | Multi-tap/bucket power preserved without forcing list syntax on the common single-target case. Defaults are values that apply to every list entry downstream — a list at defaults level has no semantic meaning. |
| **DEC-4** | `defaults.crates:` is valid only when top-level `crates:` is used. `defaults.workspaces:` is valid only when top-level `workspaces:` is used. Mismatch is **parse-time error** (no silent ignore). | Matches deny-unknown-fields ethos: fail loud on misconfig. Eliminates "where does my default live?" ambiguity. |
| **DEC-5** | All **anodizer-internal** renames / deprecations are **hard-break, no aliases, no deprecation warnings**. Update anodizer config structs + both `.anodizer.yaml` files (anodizer + cfgd) + all docs in one diff per item. | Only consumers of anodizer-internal field history are anodizer's own YAML + cfgd's YAML, both user-owned. |
| **DEC-6** | Canonical publisher toggle field name is **`skip:`** (matches GR canonical, anodizer CLI vocabulary, verb-as-instruction semantic). Rename 24 anodizer `disable:` sites; 5 sites already use `skip:`. | One vocabulary across GR config, anodizer config, anodizer CLI. |
| **DEC-7** | Canonical type for `env:` fields is **`Vec<String>`** (`"KEY=VAL"` entries) across all 8 sites (matches GR `[]string` exactly; preserves ordering for sign/sbom/notarize chains). Drop the `HashMap` shape and the dual-form deserializer. | One shape across anodizer; matches GR; fixes ordering bug at sign+sbom. |
| **DEC-8** | No GR-config migration aliases. Use GR's canonical names directly. | New repo; nobody ports a Go release config to a Rust release config and expects it to parse. Aliases are dead weight. |
| **DEC-9** | Defaults system merge semantics: **deep-merge maps, append+merge-by-identity for lists, `{}` = inherit-all** (suppress with `skip: true`). Defaults are fallback; per-crate wins on conflict. | The whole point of defaults is field-level reuse. List append+merge-by-identity (e.g., merge by `format:` key) lets users add archive variants without rewriting the defaults list. |
| **DEC-10** | `CargoPublishConfig` field set mirrors `cargo publish` (Cargo 1.95.0) tool surface, minus CLI-ergonomics flags and selection flags. **Surface**: `registry`, `index`, `index_timeout` (anodizer-original), `no_verify`, `allow_dirty`, `features: Vec<String>`, `all_features: bool`, `no_default_features: bool`, `target: Option<String>`, `target_dir: Option<PathBuf>`, `manifest_path: Option<PathBuf>`, `locked: bool`, `offline: bool`, `frozen: bool`, `keep_going: bool`, `jobs: Option<u32>`, `skip: Option<StringOrBool>` (peer-publisher pattern), `enabled` shorthand bool dropped (ITEM-3 hard-break). **Drop**: `--package`/`--workspace`/`--exclude` (anodizer's `crates[]` axis owns selection), `--dry-run`/`-v`/`-q`/`--color`/`--config`/`-Z` (CLI-only ergonomics, not config). | Publishers mirror their tool's surface (locked principle). Selection flags are redundant with anodizer's first-class `crates[]` axis. CLI-ergonomics flags are noise in a config file. |
| **DEC-11** | `ChocolateyConfig.tags` is `Vec<String>` only (drop the dual `String\|Vec<String>` deserializer; DEC-5 hard-break). At nuspec-emit time, anodizer joins on space (nuspec spec). | Both YAMLs already use list shape. Typed list is better UX (no whitespace ambiguity, IDE completion-friendly). GR uses single string (nuget spec native); deviation justified by ergonomics + already-implemented user choice. |
| **DEC-12** | `DockerV2.skip_push` is **dropped**. | Not used in either YAML; GR has no `skip_push` on `dockers_v2:` (only legacy `dockers:`); anodizer-additive with no consumer → DEC-5 hard-break + GR alignment. To skip publishing, use the canonical `skip:` field (DEC-6). |
| **DEC-13** | Snapcraft top-level `slots:` is **dropped**. | Not used in either YAML; GR has no top-level `slots:` (only nested under apps); anodizer-additive with no consumer → DEC-5 hard-break + GR alignment. App-scoped slots are still available via `apps.<name>.slots`. |

---

## Cross-cutting policies (LOCKED — see DEC-6/7/8)

All three former OPENs (A/B/C) ratified and rolled into DECs above. Kept here as a pointer for cross-references in SCH rows below.

- OPEN-A → **DEC-6**: `skip:` everywhere
- OPEN-B → **DEC-7**: `Vec<String>` everywhere
- OPEN-C → **DEC-8**: no aliases, GR canonical names only

---

## Structural items (from plan.md § B1–B6)

These define the shape that ITEMs in the next section land into. They sequence first.

| ITEM | Title | GR alignment | Notes |
|---|---|---|---|
| **ITEM-1** | Workspace-level defaults system (path-mirror inheritance) | **N/A — anodizer-only concept**. GR has no `crates[]` axis to inherit through; GR's solution to the same problem is top-level fan-out lists (`brews:` etc.). DEC-2 + DEC-3 codify the deviation. | Folds in: B-new-2 (`defaults.archives` field expansion). DEC-2/3/4 lock the shape; remaining design = inheritance merge semantics. |
| **ITEM-2** | `CargoPublishConfig` (was `CratesPublishConfig`) full field expansion | **N/A — no GR equivalent** (Go ecosystem, no crates.io publisher). Field set should mirror `cargo publish` CLI surface (analogous to how GR's publishers mirror their tool's surface). | Folds in: B-new-12. Decision content = which `cargo publish` flags surface. |
| **ITEM-3** | Publisher-key rename: `publish.crates:` → `publish.cargo:` | **N/A — anodizer-original publisher**. Naming choice (`cargo`, the manager binary) follows GR's pattern (publisher key = manager tool name). | Locked name + hard-break (DEC-5). Drop the `crates: true\|false` bool shorthand at the same time. |
| **ITEM-4** | Cask struct unification (`HomebrewCaskConfig` ∪ `TopLevelHomebrewCaskConfig`) | **Partial — match GR shape, deviate on placement**. GR has only top-level `homebrew_casks:` (one struct). Anodizer has both top-level + per-crate (per ITEM-1 inheritance). Unification matches GR's struct shape but anodizer keeps both call-sites for crates-axis fit. | Folds in: nothing in (b) list directly; depends on ITEM-1 inheritance shape. |
| **ITEM-5** | Both-YAML migration (`/opt/repos/anodizer/.anodizer.yaml` + `/opt/repos/cfgd/.anodizer.yaml`) | **N/A — implementation, not a schema decision.** | Last; consumes the locked shapes from all other ITEMs. |

---

## Schema items (the 33 (b) findings, deduped)

Numbering: SCH-N. Where two findings collapse to one decision, the duplicate is flagged `dupe-of:` and the canonical SCH owns the work.

### Build / archive / source (4 (b) from audit 1)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-1** | `BuildConfig.flags: Option<String>` mis-splits quoted args. Change to `Vec<String>`. (No back-compat string-adapter — DEC-5 hard-break.) | audit 1 #1, B-new-1 | type-change | — |
| **SCH-2** | `defaults.archives` only carries `format` + `format_overrides`; needs `name_template`, `formats`, `wrap_in_directory`, `builds_info`. | audit 1 #14, B-new-2 | defaults-expansion | **ITEM-1** (folded — pure defaults work) |
| **SCH-3** | `SourceFileInfo.mode: Option<u32>` vs `ArchiveFileInfo.mode: Option<String>` — type unification across two near-identical types. | audit 1 #20, B-new-3 | type-unify | — |
| **SCH-4** | `CrateConfig.docker` legacy alongside `docker_v2` — deprecate / remove / warn-on-use. | audit 1 #26, B-new-4 | hard-break (DEC-5) | — |

### Docker / nfpm / installers (7–8 (b) from audit 2)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-5** | Top-level YAML alias gaps: `nfpms`/`dmg`/`msi`/`flatpak` missing `#[serde(alias = ...)]`. | audit 2 #1, plan B5 | alias | — |
| **SCH-6** | `DockerV2.skip_push` — anodizer-additive (no GR equivalent on `dockers_v2:`). Decide keep+document or remove. | audit 2 #3, B-new-5 | additive-decision | — |
| **SCH-7** | `NfpmConfig.umask` String-only; GR allows int OR string. Add `StringOrU32` deserializer. | audit 2 #5, plan B5 | type-coercion | — |
| **SCH-8** | NfpmContent vs NfpmContentConfig DRY — same fields, two structs. | audit 2 #9, plan B5 | DRY-merge | — |
| **SCH-9** | NfpmSignatureConfig vs SrpmSignatureConfig DRY — same fields, two structs. | audit 2 #10, plan B5 | DRY-merge | — |
| **SCH-10** | Snapcraft top-level `slots` field — anodizer-only (no GR equivalent at top). Decide keep+document or remove. | audit 2 #11, B-new-6 | additive-decision | — |
| **SCH-11** | Makeself `filename` alias gap. | audit 2 #14, plan B5 | alias | — |
| **SCH-12** | SrpmConfig 7 missing RPM-spec fields: `Bins`, `ImportPath`, `Prefixes`, `BuildHost`, `Pretrans`, `Posttrans`, `Prerelease`, `VersionMetadata`. | audit 2 #15, plan B5 | field-add | — |

### Publishers / package managers (11 (b) from audit 3)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-13** | Homebrew legacy `commit_author_name`/`commit_author_email` parallel to structured `commit_author`. | audit 3 #7, B-new-7 | hard-break (DEC-5) | — |
| **SCH-14** | Scoop legacy `bucket: BucketConfig` parallel to `repository: RepositoryConfig`. | audit 3 #9, B-new-8 | hard-break (DEC-5) | **dupe-of: SCH-21** (one row of the legacy-repo-struct sweep) |
| **SCH-15** | Chocolatey `tags` accepts `Vec<String>` OR space-separated string — canonicalize to `Vec<String>` only (DEC-5 hard-break). | audit 3 #14, B-new-9 | type-change | — |
| **SCH-16** | AurConfig `url` legacy redundant with `homepage`. | audit 3 #17, B-new-10 | hard-break (DEC-5) | — |
| **SCH-17** | AurSourceConfig MISSING `amd64_variant` (GR has `Goamd64`). | audit 3 #22, plan B5 | field-add | — |
| **SCH-18** | KrewConfig `manifests_repo: KrewManifestsRepoConfig` and `upstream_repo: KrewManifestsRepoConfig` legacy — both die, only `repository: RepositoryConfig` survives. | audit 3 #24, B-new-11 | hard-break (DEC-5) | **dupe-of: SCH-21** (one row of the legacy-repo-struct sweep; covers both krew legacy fields) |
| **SCH-19** | `CratesPublishConfig` `enabled: bool` inconsistent with peer publishers. | audit 3 #29, B-new-12 | type-change | **ITEM-2** (folded — covered by full expansion) |
| **SCH-20** | `CloudSmithConfig.skip` only — inconsistent with peer publishers. Action depends on OPEN-A (canonical name). | audit 3 #31, B-new-13 | rename / hard-break (DEC-5) | resolved by OPEN-A |
| **SCH-21** | Legacy `{owner, name}` structs: `TapConfig`, `BucketConfig`, `ChocolateyRepoConfig`, `WingetManifestsRepoConfig`, `KrewManifestsRepoConfig` → unify on `RepositoryConfig` (has token/branch/git/pull_request fields too). | audit 3 #32, B-new-14 | DRY-merge + hard-break (DEC-5) | — |
| ~~SCH-22~~ | ~~No per-publisher CLI skip flags (`--skip=brew`, etc.)~~ — **EXCLUDED**: pure CLI surface, no schema change required. Belongs to a CLI-UX session. | audit 3 #33, B-new-15 | — | — |
| **SCH-23** | Top-level `brews:`/`scoops:`/`wingets:`/`chocolateys:`/`aurs:`/`nix:`/`krews:` multi-publisher pattern (GR parity). | audit 3 #34, B-new-16 | additive-decision | **DROPPED** by DEC-2 + DEC-3 (replaced by list-per-crate at scoped path) |

### Changelog / milestone (3 (b) from audit 4)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-24** | Per-entry `Authors`/`Logins` template field — `Logins` global today; expose per-entry. | audit 4 C8, B-new-19 | field-add | — |
| **SCH-25** | Changelog `header`/`footer: String` → `ContentSource` (asymmetric with release block, which uses ContentSource). | audit 4 C11, B-new-17 | type-change | — |
| **SCH-26** | Changelog snapshot-always-on → opt-in `changelog.snapshot: bool`. | audit 4 C12, B-new-18 | behavior toggle | — |

### Checksum / sign / notarize / sbom (6–7 (b) from audit 5)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-27** | `binary_signs` reuses `SignConfig`; `artifacts` not enum-constrained to `binary\|none`. Surface as separate type or jsonschema enum. | audit 5 S1, B-new-20 | type-constraint | — |
| **SCH-28** | `signs.env: HashMap<String,String>` loses ordering, can't chain envs. Resolved by OPEN-B (env-field type policy). | audit 5 S12, B-new-21 | type-change | resolved by OPEN-B |
| **SCH-29** | Notarize `timeout: String` → typed `Duration` (with serde). | audit 5 N1, B-new-22 | type-change | — |
| **SCH-30** | Notarize top-level `disable` + per-cfg `enabled` doubled surface — consolidate. | audit 5 N7, B-new-23 | DRY-merge + hard-break (DEC-5) | — |
| **SCH-31** | `NotarizeConfig.macos_native.use_` no enum constraint — add jsonschema enum. | audit 5 N9, B-new-24 | type-constraint | — |
| **SCH-32** | `SbomConfig.env: HashMap` loses ordering — same fix as SCH-28. | audit 5 B1+B7, B-new-25 | type-change | resolved by OPEN-B (one design across 8 env sites) |

### Infra publishers / announcers (1+1 (b) from audits 6, 7)

| SCH | Finding | Source | Tag | Folds into |
|---|---|---|---|---|
| **SCH-33** | `UploadConfig.disable` rename to align with canonical name from OPEN-A. | audit 6 U1, B-new-26 | rename / hard-break (DEC-5) | resolved by OPEN-A |
| **SCH-34** | `AnnounceConfig.email` add `#[serde(alias = "smtp")]` for GR-migration compat. | audit 7 AN25, B-new-27 | alias | — |

---

## GR alignment (per SCH)

Compact table mapping each SCH to its GoReleaser baseline and proposed alignment. The walk uses this as the starting point for the per-item GR-baseline field. Items marked `verify` need GR-source confirmation at walk time.

Legend:
- ✅ **match GR** — proposed shape matches GR.
- ⚠️ **deviate** — reason in cell.
- ▫️ **N/A** — anodizer-only concept; no GR baseline applies.

| SCH | GR baseline | Alignment |
|---|---|---|
| SCH-1 | GR uses `[]string` for build flags | ✅ match — `Vec<String>` |
| SCH-3 | GR uses `uint32` for file mode (single canonical type) | ✅ match — unify to `u32` |
| SCH-4 | GR uses `dockers:` (legacy) and `dockers_v2:` (new); GR is keeping both | ⚠️ deviate — anodizer drops legacy `docker:` per DEC-5 (only cfgd consumes; clean break OK) |
| SCH-5 | GR has `nfpms:` (plural), `dmg:`, `msi:`, `flatpaks:` as canonical | Match GR plural canonical OR add as alias — depends on OPEN-C (GR-config migration) |
| SCH-6 | GR has no `skip_push` on `dockers_v2:` (only on legacy `dockers:`) | ⚠️ deviate (current state) — KEEP requires reason; remove for GR alignment |
| SCH-7 | GR's `Umask` accepts int OR string in YAML | ✅ match — add `StringOrU32` deserializer |
| SCH-8 | GR has one `NfpmContent` type | ✅ match — DRY merge to one struct |
| SCH-9 | GR has separate `NfpmSignature` and `SrpmSignature` (different field sets) | ⚠️ verify — confirm field divergence at walk; merge only if GR has converged |
| SCH-10 | GR has no top-level `slots:` on snapcraft (only nested under apps) | ⚠️ deviate (current state) — remove for GR alignment unless feature is load-bearing |
| SCH-11 | GR has `Makeself.Filename` field (no alias needed there) | Match GR field name OR add alias — depends on OPEN-C |
| SCH-12 | GR has all 7 fields (`Bins`, `ImportPath`, etc.) on `NFPM`/`Srpm` | ✅ match — add the missing fields |
| SCH-13 | GR has only structured `CommitAuthor` (no flat legacy fields) | ✅ match — drop legacy flat fields |
| SCH-15 | GR has `Tags string` (single string, space-separated) | ⚠️ deviate — `Vec<String>` is better UX (typed list); deviation justified by ergonomics |
| SCH-16 | GR has both `URL` and `Homepage` on AUR (different meanings) | ⚠️ verify — check whether anodizer's `url` is genuinely redundant or accidentally aliased |
| SCH-17 | GR has `Goamd64` on AurSource | ✅ match — add `amd64_variant` field (Rust-target-triple equivalent) |
| SCH-21 | GR has separate `RepoRef`-shaped structs per publisher (Tap, Bucket, etc.); fields are similar but typed distinctly | ⚠️ deviate — anodizer's `RepositoryConfig` already supersedes them with token/branch/git/pull_request; unification is a cohesion win, GR-ergonomics loss is small |
| SCH-24 | GR exposes per-entry `Authors`/`Logins` in changelog templates | ✅ match — expose per-entry |
| SCH-25 | GR uses `string` for changelog `Header`/`Footer` | ⚠️ deviate — `ContentSource` matches anodizer's release block (symmetry); GR is asymmetric here |
| SCH-26 | GR renders changelog every run (effectively always-on) | ⚠️ verify — confirm GR's actual behavior; anodizer's opt-in deviates if true |
| SCH-27 | GR has `SignArtifacts` constants set (binary, all, none, etc.) | ✅ match — enum-constrain |
| SCH-29 | GR uses `time.Duration` for notarize timeout | ✅ match — typed `Duration` |
| SCH-30 | GR has only `Disable` (no `Enabled` doubled) on Notarize | ✅ match — drop one |
| SCH-31 | GR validates `MacOS.Use` as enum (`notarytool` only currently) | ✅ match — enum-constrain |
| SCH-34 | GR renamed `smtp:` → `email:` in v1.21+ with alias compat | ✅ match — alias mirrors GR's own migration story; subject to OPEN-C |
| **SCH-2** (folded → ITEM-1) | GR has `defaults.archives:` only on builds, not full archive surface | ⚠️ deviate — defaults expansion is anodizer-architectural |
| **SCH-14** (dupe → SCH-21) | see SCH-21 | see SCH-21 |
| **SCH-18** (dupe → SCH-21) | see SCH-21 | see SCH-21 |
| **SCH-19** (folded → ITEM-2) | GR has no Cargo publisher | ▫️ N/A — anodizer-original |
| **SCH-20** (resolved by OPEN-A) | see OPEN-A | see OPEN-A |
| **SCH-28** (resolved by OPEN-B) | see OPEN-B | see OPEN-B |
| **SCH-32** (resolved by OPEN-B) | see OPEN-B | see OPEN-B |
| **SCH-33** (resolved by OPEN-A) | see OPEN-A | see OPEN-A |

Items marked `verify` (5: SCH-9, SCH-10 reason check, SCH-11 alias decision, SCH-16, SCH-26) need GR source/docs lookup at walk time.

---

## Excluded items (with reason)

| Original ID | Reason for exclusion |
|---|---|
| `_session-b-inputs.md` line 22 — HomebrewConfig.commit_msg_template doc/code drift | Already landed in Session A as (d) batch (`9505686`). |
| `_session-b-inputs.md` line 23 — WingetConfig.package_identifier regex | Already landed in Session A as (a) (`9505686`). |
| `_session-b-inputs.md` line 24 — KrewConfig description-required validation | Already landed in Session A as (a) (`f43ce2f`). |
| `_session-b-inputs.md` line 16 — DockerV2.SBOM default flip | Behavior, not schema → **Session C3**. Cross-link only, not in this scope. |
| **B-new-16 / SCH-23** — top-level `brews:`/`scoops:`/etc. multi-publisher fan-out | Dropped by DEC-2 + DEC-3 (multi-target now via list-per-crate at `crates[].publish.<pub>:`). Original power preserved via different shape. |

---

## Session B follow-on (non-schema, MUST ship with Session B)

These items are not config-schema decisions but depend on Session B's renames/shapes and would silently drift if pushed to a separate session. They land in the same implementation wave as the ITEM/SCH work, sequenced after their dependencies.

| FOLL | Item | Depends on | Why bundled here |
|---|---|---|---|
| **FOLL-1** | Add per-publisher CLI skip flags: `--skip=brew`, `--skip=scoop`, `--skip=cargo`, `--skip=choco`, `--skip=winget`, `--skip=krew`, `--skip=nix`, `--skip=aur`. (Originally SCH-22.) | **ITEM-3** (cargo rename) | Flags accept canonical publisher names. If they ship before the rename, `--skip=crates` would be added then immediately renamed. Bundle for cohesion. |

---

## Dupes summary

Internal dupes flagged inline above. Net dupe count after review: **5**:
- SCH-2 folded into ITEM-1 (defaults expansion = pure defaults work)
- SCH-19 folded into ITEM-2 (covered by full Cargo expansion)
- SCH-14 folded into SCH-21 (Scoop bucket = one row of legacy-repo-struct sweep)
- SCH-18 folded into SCH-21 (Krew manifests/upstream = one row of same sweep, both legacy fields die)
- SCH-32 resolved by OPEN-B (8-site env policy, one design)

Plus two SCHs newly resolved by cross-cutting policies:
- SCH-20 + SCH-33 → resolved by OPEN-A (canonical toggle field name, one rename across all sites)
- SCH-28 + SCH-32 → resolved by OPEN-B (canonical env-field type, one design across 8 sites)

---

## Counts

- Pre-walk decisions locked: **5** (DEC-1..5)
- Cross-cutting policies open (walk first): **2** (OPEN-A, OPEN-B)
- Structural items: **5** (ITEM-1..5)
- Schema items in scope: **31** (34 listed; 2 excluded as DROPPED + EXCLUDED, 1 reclassified to FOLL-1)
- Schema items folded into structural / dupe-of-other / resolved by OPEN: **9** (SCH-2, SCH-14, SCH-18, SCH-19, SCH-20, SCH-28, SCH-32, SCH-33; plus SCH-25 standalone)
- Schema items requiring stand-alone walk: **22** (the rest)
- Session B follow-on (non-schema, ships with Session B): **1** (FOLL-1)

The 33 vs ~34 discrepancy traced: audit 5 lists B1 + B7 as separate (b) marks, but B7 is the cross-cut "same as S12 + B1" — so categorization counts them as one. Inventory consolidates to SCH-28 + SCH-32 (both now resolved by OPEN-B).

---

## Walk format (per item)

For each topic the walk presents:
1. **Current shape** — Rust struct excerpt + actual YAML excerpt from anodizer or cfgd.
2. **Proposed shape** — same excerpts, after the change.
3. **GR baseline** — what GoReleaser does (link to GR source / docs when relevant). REQUIRED for every decision.
4. **Decision** — exactly one (or, for batches, one per row).
5. **GR alignment** — does the proposed shape match GR? If deviating, **why** the deviation is justified (cohesion, ergonomics, anodizer-only concept, etc.). REQUIRED.
6. **Trade-offs** — bullets, what is gained / lost / unchanged.
7. **STOP** — wait for ratification before moving on.

For batch topics (alias batch, type-change batch, etc.) a single walk shows all rows with one decision each; user can ratify all at once or per row.

**Default policy on GR alignment**: match GR unless there's a specific cohesion/ergonomics/anodizer-architecture reason to deviate. Each deviation must carry its reason in the locked outcome.

---

## Walk status — CLOSED 2026-04-27

All decisions locked. **DEC-1..13** above cover every cross-cutting policy and the four post-compact items (ITEM-2 cargo flags = DEC-10, SCH-15 chocolatey tags = DEC-11, SCH-6 = DEC-12, SCH-10 = DEC-13). Per-SCH GR alignment table covers the rest.

Plan artifact for the implementation session: **[`_session-b-plan.md`](_session-b-plan.md)**.
