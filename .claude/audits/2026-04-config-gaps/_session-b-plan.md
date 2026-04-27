# Session B implementation plan — config-schema gaps

Date: 2026-04-27
Source of decisions: [`_session-b-inventory.md`](_session-b-inventory.md) (DEC-1..13).
Source of findings: [`_categorization.md`](_categorization.md) (b-bucket, 33 items).
Wave gating: per-batch `task commit` (lint → build → clippy → docs → dry-run-release).

This plan is the executable artifact for the implementation session. Every item below has its decision locked. **No design questions remain.** The implementation session reads this top-to-bottom.

---

## Operating rules (carried in from inventory)

1. **DEC-5 hard-break**: anodizer's only consumers are `/opt/repos/anodizer/.anodizer.yaml` and `/opt/repos/cfgd/.anodizer.yaml`. No deprecation aliases, no warning-mode shims, no parallel-shape support. Update structs + both YAMLs + all docs in one wave per batch.
2. **DEC-8 no GR-config aliases**: use GR's canonical names directly (no migration aliases for GR users).
3. **GR-alignment default**: match GR unless a deviation reason is locked in the inventory's `## GR alignment (per SCH)` table.
4. `serde(default, deny_unknown_fields)` already enforced project-wide (Session A baseline) — don't loosen.
5. **`task commit`** per wave. **No** `git commit`. **Never** push.
6. After each wave: extend `crates/core/src/config_tests/` snapshots and update `xtask gen-docs` outputs in the same commit (lint precondition will block otherwise).
7. Plan/spec changes go in the commit body, not as a session-note source comment (memory: `feedback_no_session_note_comments`).

---

## Wave order (with dependencies)

```
WAVE 1  ─ Cross-cutting renames        ─ DEC-6, DEC-7
WAVE 2  ─ Defaults system foundation   ─ ITEM-1, DEC-2/3/4/9
WAVE 3  ─ CargoPublishConfig + rename  ─ ITEM-2, ITEM-3, FOLL-1, DEC-1/10
WAVE 4  ─ Cask unification             ─ ITEM-4
WAVE 5  ─ Schema item batches          ─ SCH-1..34 (excluding folded/dropped)
WAVE 6  ─ Both-YAML migration          ─ ITEM-5 (consumes all locked shapes)
```

Why this order:
- WAVE 1 first because every later wave touches structs that contain `disable:` / `env:` fields. Doing the global rename first means later waves write the canonical names.
- WAVE 2 second because ITEM-2 / ITEM-4 / SCH-2 (folded into ITEM-1) all consume defaults inheritance.
- WAVE 3 before WAVE 5 because FOLL-1 (CLI flags) accepts the renamed publisher key.
- WAVE 6 last so the YAMLs land against the final shape (no churn).

Each wave below is one `task commit`. Inside a wave, file edits can fan out; the commit is atomic.

---

## WAVE 1 — Cross-cutting renames

**Decisions consumed**: DEC-6 (`disable:` → `skip:`), DEC-7 (`env:` HashMap → Vec<String>), DEC-8 (no aliases).

### W1.1 — `disable:` → `skip:` (24 sites)

**Action**: rename the field in every Rust struct that exposes `disable: Option<StringOrBool>`. Keep the `Option<StringOrBool>` type. **No alias** (DEC-5).

**Sites** (line numbers from `crates/core/src/config.rs` at HEAD `cd8319c`; verify by grep at execution time):

```
1476, 1591, 2776, 2828, 2881, 3013, 3036, 3514, 3687, 3720, 3767,
3797, 3837, 3877, 3930, 3987, 4253, 4334, 5141, 5281, 5783, 5898,
5996, 6060
```

**Already canonical** (5 sites kept): `1044`, `4724`, `5222`, `5250`. *(Plus any `skip` introduced by WAVE 3.)*

**Verify** before wave commit:

```bash
$ grep -nE "pub disable: Option<StringOrBool>" crates/core/src/config.rs
# (no output expected)
$ grep -cE "pub skip: Option<StringOrBool>" crates/core/src/config.rs
# expect ≥ 28
```

**YAML migration (in same commit)**: rewrite both `.anodizer.yaml` files. `disable: true` → `skip: true` everywhere. Affected blocks: `flatpaks`, `app_bundles`, `dmgs`, `pkgs`, `nfpms`, `snapcrafts`, `dockers`/`dockers_v2`, `homebrews`, `scoops`, `chocolateys`, `wingets`, `aurs`, `nix`, `krews`, `notarize`, `sbom`, `sign`, `binary_signs`, `docker_signs`, `changelog`, `cloudsmith`, `upload`, `srpms`, `makeselves`. Use `git diff` to verify no `disable:` survives.

### W1.2 — `env:` HashMap → Vec<String> (8 sites)

**Action**: change the field type. Drop the dual-form deserializer if one exists.

**Sites**:

```
crates/core/src/config.rs:119, 872, 4242, 4496, 4531, 5275, 5323, 5515
```

**Excluded** (different shape, NOT in DEC-7 scope):
- Line `1052` — per-target nested `HashMap<String, HashMap<String, String>>` (build-level). Different concept; leave as-is.

**Type change**:

```rust
// before
pub env: Option<HashMap<String, String>>,
// after
pub env: Option<Vec<String>>,  // entries are "KEY=VAL"
```

**Wiring (in same commit)**: any reader of these fields must consume the `Vec<String>` shape. Search for `.env.as_ref()` and `.env.iter()` in stage crates and update. Likely affected:
- `crates/stage-build/src/**` (build env)
- `crates/stage-sign/src/**` (sign env, sbom env)
- `crates/stage-notarize/src/**`
- `crates/stage-archive/src/**` (defaults env)
- `crates/stage-publish/src/**` (cloudsmith env)
- `crates/stage-blob/src/**` (upload env)

**No YAML changes needed** — both YAMLs already use list-of-`KEY=VAL` shape via the dual-form deserializer; the shape on disk does not change, only the Rust type.

**Verify**:

```bash
$ grep -nE "env: Option<HashMap<String, String>>" crates/core/src/config.rs
# (no output expected)
$ cargo test -p core env  # parsing tests for env coercion
```

---

## WAVE 2 — Defaults system foundation (ITEM-1)

**Decisions consumed**: DEC-2 (defaults shadow config; no parallel fan-out), DEC-3 (list-or-scalar at per-crate, single-struct at defaults), DEC-4 (defaults-axis-mismatch is parse error), DEC-9 (deep-merge maps, append+merge-by-identity for lists, `{}` = inherit-all, `skip: true` to suppress).

### W2.1 — Expand `Defaults` struct to mirror `CrateConfig` shape

**Current shape** (`crates/core/src/config.rs:821`):

```rust
pub struct Defaults {
    pub archives: Option<DefaultArchiveConfig>,  // narrow shape — line 840
    pub targets:  Option<Vec<String>>,
    pub env:      Option<Vec<String>>,           // post-W1.2
    // ...sparse subset of CrateConfig
}
```

**Proposed shape** (path-mirror inheritance):

```rust
pub struct Defaults {
    // Build axis
    pub builds:        Option<BuildConfig>,
    pub archives:      Option<ArchiveConfig>,        // full shape, was DefaultArchiveConfig
    pub source:        Option<SourceConfig>,
    pub upx:           Option<UpxConfig>,

    // Packaging axis
    pub nfpms:         Option<NfpmConfig>,
    pub snapcrafts:    Option<SnapcraftConfig>,
    pub flatpaks:      Option<FlatpakConfig>,
    pub app_bundles:   Option<AppBundleConfig>,
    pub dmgs:          Option<DmgConfig>,
    pub pkgs:          Option<PkgConfig>,
    pub msis:          Option<MsiConfig>,
    pub nsis:          Option<NsisConfig>,
    pub makeselves:    Option<MakeselfConfig>,
    pub srpms:         Option<SrpmConfig>,
    pub docker_v2:     Option<DockerV2Config>,

    // Publish axis
    pub publish:       Option<PublishDefaults>,      // see W2.2

    // Sign / notarize / sbom
    pub sign:          Option<SignConfig>,
    pub binary_signs:  Option<SignConfig>,
    pub docker_signs:  Option<DockerSignConfig>,
    pub notarize:      Option<NotarizeConfig>,
    pub sbom:          Option<SbomConfig>,

    // Cross-cutting
    pub targets:       Option<Vec<String>>,
    pub env:           Option<Vec<String>>,

    // Crate-axis vs workspace-axis (mutually exclusive — DEC-4)
    pub crates:        Option<DefaultsCrateBlock>,
    pub workspaces:    Option<DefaultsWorkspaceBlock>,
}
```

**`PublishDefaults`** = mirror of `crates[].publish` but with single-struct-per-publisher (DEC-3):

```rust
pub struct PublishDefaults {
    pub homebrew:   Option<HomebrewConfig>,
    pub cargo:      Option<CargoPublishConfig>,   // DEC-1 rename, populated in WAVE 3
    pub scoop:      Option<ScoopConfig>,
    pub winget:     Option<WingetConfig>,
    pub chocolatey: Option<ChocolateyConfig>,
    pub krew:       Option<KrewConfig>,
    pub nix:        Option<NixConfig>,
    pub aur:        Option<AurConfig>,
    pub aur_source: Option<AurSourceConfig>,
}
```

**Per-crate counterpart** (DEC-3 — list-or-scalar via untagged enum):

```rust
pub struct CratePublishConfig {
    pub homebrew:   Option<OneOrMany<HomebrewConfig>>,
    pub cargo:      Option<OneOrMany<CargoPublishConfig>>,
    pub scoop:      Option<OneOrMany<ScoopConfig>>,
    // ...etc
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}
```

`OneOrMany<T>` lives in `crates/core/src/serde_helpers.rs` (next to `StringOrBool`).

### W2.2 — Inheritance merge engine

**Decision (DEC-9)**:
- Maps: deep-merge (per-crate field wins on conflict, defaults fill gaps).
- Lists: append + merge-by-identity. Identity key:
  - `archives[]` → `format`
  - `nfpms[]` → `id` (or first of `id`, `package_name`)
  - `snapcrafts[]` → `name`
  - `flatpaks[]` / `app_bundles[]` / `dmgs[]` / `pkgs[]` / `msis[]` / `nsis[]` / `makeselves[]` / `srpms[]` → `id` if present, else positional
  - `docker_v2[]` → `id`
  - publisher lists (under `OneOrMany::Many`) → `id` if present, else positional
- `{}` (empty map) at per-crate position = inherit-all-from-defaults (no override).
- `skip: true` at per-crate position = suppress the inherited block entirely.

**Module**: new file `crates/core/src/defaults_merge.rs`. Pure-function `apply_defaults(defaults: &Defaults, crate_cfg: &mut CrateConfig)`. Called from the existing `Config::resolve()` path (or equivalent post-parse hook).

**Tests** (in `crates/core/src/defaults_merge_tests.rs`):
- map deep-merge: defaults set `signing.gpg.key`, crate sets `signing.gpg.passphrase` → both survive
- list append: defaults `archives: [{format: tar.gz}]`, crate `archives: [{format: zip}]` → both
- list merge-by-identity: defaults `archives: [{format: tar.gz, name_template: "X"}]`, crate `archives: [{format: tar.gz, files: [...]}]` → one entry, fields combined, crate wins on `name_template`
- `{}` = inherit-all
- `skip: true` = suppress
- per-crate scalar wins over defaults scalar on conflict

### W2.3 — Axis-mismatch parse error (DEC-4)

**Decision**:
- `defaults.crates:` valid only when top-level `crates:` is present.
- `defaults.workspaces:` valid only when top-level `workspaces:` is present.
- Both forms or wrong-form → parse-time error with span pointing at the offending field.

**Implementation**: post-deserialization validator in `Config::validate()`. Error variant `ConfigError::DefaultsAxisMismatch { used: &'static str, expected: &'static str }`.

### W2.4 — Drop `DefaultArchiveConfig` (folds SCH-2)

`DefaultArchiveConfig` (line 840) is the narrow precursor. After W2.1 it is replaced by `Option<ArchiveConfig>`. Delete the type and its tests.

### W2.5 — Both-YAML lint check

After W2.1–W2.4, run a parse against both YAMLs to confirm no breakage:

```bash
$ cargo run --bin anodizer -- validate --config /opt/repos/anodizer/.anodizer.yaml
$ cargo run --bin anodizer -- validate --config /opt/repos/cfgd/.anodizer.yaml
```

(YAMLs are not yet rewritten to use new defaults — that is WAVE 6. They must continue to parse with the empty/sparse defaults they have today.)

---

## WAVE 3 — Cargo publisher rename + flag expansion

**Decisions consumed**: DEC-1 (`crates:` → `cargo:`), DEC-10 (cargo-publish field set), DEC-5 (hard-break), FOLL-1 (CLI flags), ITEM-3 (drop `enabled` shorthand bool).

### W3.1 — Rename type and field

```rust
// before
pub struct CratesPublishConfig { /* ... */ }
pub struct CratePublishConfig {
    pub crates: Option<CratesPublishConfig>,
}
// after
pub struct CargoPublishConfig { /* expanded — see W3.2 */ }
pub struct CratePublishConfig {
    pub cargo: Option<OneOrMany<CargoPublishConfig>>,  // OneOrMany from W2.1
}
```

`PublishDefaults.cargo` (W2.1) = `Option<CargoPublishConfig>` (single struct per DEC-3).

### W3.2 — Expand `CargoPublishConfig` (DEC-10)

```rust
pub struct CargoPublishConfig {
    // Registry selection
    pub registry:            Option<String>,
    pub index:               Option<String>,
    pub index_timeout:       Option<u64>,           // anodizer-original

    // Verify / dirty
    pub no_verify:           Option<bool>,
    pub allow_dirty:         Option<bool>,

    // Feature selection
    pub features:            Option<Vec<String>>,
    pub all_features:        Option<bool>,
    pub no_default_features: Option<bool>,

    // Compilation
    pub target:              Option<String>,
    pub target_dir:          Option<PathBuf>,
    pub jobs:                Option<u32>,
    pub keep_going:          Option<bool>,

    // Manifest
    pub manifest_path:       Option<PathBuf>,
    pub locked:              Option<bool>,
    pub offline:             Option<bool>,
    pub frozen:              Option<bool>,

    // Peer-publisher pattern
    pub skip:                Option<StringOrBool>,
}
```

**Dropped flags** (and why):
- `--package` / `--workspace` / `--exclude` — anodizer's `crates[]` axis owns selection.
- `--dry-run` — CLI ergonomics, not config.
- `-v` / `-q` / `--color` — CLI output ergonomics.
- `--config` / `-Z` — Cargo CLI escape hatches, not config-file material.

### W3.3 — Drop `crates: bool\|object` shorthand (ITEM-3)

The current `crates: true` / `crates: { enabled: true }` shorthand goes away. Only the new shape parses:

```yaml
publish:
  cargo: { skip: false }    # opt-in (or omit since default)
  cargo: { skip: true }     # opt-out
```

`enabled` field is removed from the type (was `SCH-19`, folded). To-skip path is canonical `skip:` (DEC-6).

### W3.4 — Stage wiring

`crates/stage-publish/src/cargo*.rs` (or whatever the current cargo publisher source is — likely renamed from `crates*.rs` in this same commit) must:
- Accept the new `CargoPublishConfig`.
- Pass each populated field to the underlying `cargo publish` subprocess invocation.
- Honor `skip:` at the resolution step (peer-publisher convention).

### W3.5 — CLI flag rename + new flags (FOLL-1)

In `crates/cli/src/`:
- Replace `--skip=crates` with `--skip=cargo`.
- Add per-publisher `--skip=` values: `brew`, `scoop`, `cargo`, `choco`, `winget`, `krew`, `nix`, `aur`. (`brew` not `homebrew` to match GR + brew CLI binary.)
- Update help text and shell-completion outputs.

**Verify**:

```bash
$ anodizer release --skip=cargo --dry-run    # accepted
$ anodizer release --skip=crates --dry-run   # rejected with "did you mean cargo?"
```

(The "did you mean" hint is a one-line suggestion-on-unknown-value addition; if the CLI framework doesn't make it free, drop it — DEC-5 is hard-break, no compat hint required.)

### W3.6 — YAML migration (anodizer-only in this wave)

The 16 `crates[]` entries in `/opt/repos/anodizer/.anodizer.yaml` each carry `publish: { crates: { enabled: true } }`. Rewrite to:

```yaml
publish:
  cargo: {}    # opt-in via presence; defaults pull rest from defaults.publish.cargo
```

(Or simply `publish: { cargo: {} }`.) The `defaults.publish.cargo:` block is added in WAVE 6.

`cfgd/.anodizer.yaml` does not currently configure cargo publishing; if it does, mirror.

---

## WAVE 4 — Cask unification (ITEM-4)

**Decision**: collapse `HomebrewCaskConfig` and `TopLevelHomebrewCaskConfig` into one `HomebrewCaskConfig`. Both call sites (top-level `homebrew_casks:` and per-crate `publish.homebrew_cask:`) deserialize the same struct. Per-crate honors DEC-3 (list-or-scalar via `OneOrMany`).

**Action**:
- Delete `TopLevelHomebrewCaskConfig`.
- Promote `HomebrewCaskConfig` to carry every field from both originals.
- Wire `Defaults.publish.homebrew_cask: Option<HomebrewCaskConfig>` (single struct per DEC-3).
- Wire `CratePublishConfig.homebrew_cask: Option<OneOrMany<HomebrewCaskConfig>>`.
- Stage code (`crates/stage-publish/src/homebrew_cask.rs`): unify its two consumption paths into one.

**No YAML in either repo currently uses casks** — confirm via:

```bash
$ grep -nE "homebrew_cask" /opt/repos/anodizer/.anodizer.yaml /opt/repos/cfgd/.anodizer.yaml
```

If empty, no YAML change needed in this wave.

---

## WAVE 5 — Schema item batches

Each batch below is a single `task commit` unless flagged otherwise. Items decided by GR-alignment + DEC table; no questions remain.

### W5.1 — Type-coercion batch

| SCH | File / line | Change |
|---|---|---|
| **SCH-1** | `BuildConfig.flags` (`config.rs:1056`) | `Option<String>` → `Option<Vec<String>>`. No back-compat string-adapter. |
| **SCH-3** | `SourceFileInfo.mode` + `ArchiveFileInfo.mode` | unify both to `Option<u32>`. Match GR. |
| **SCH-7** | `NfpmConfig.umask` | `Option<String>` → custom `StringOrU32` deserializer (`crates/core/src/serde_helpers.rs`). Match GR (int OR string). |
| **SCH-25** | `ChangelogConfig.header`, `ChangelogConfig.footer` | `Option<String>` → `Option<ContentSource>`. Symmetric with release block. Deviates from GR string; reason: anodizer-internal symmetry. |
| **SCH-29** | `NotarizeConfig.timeout` | `Option<String>` → `Option<Duration>` with serde-friendly `humantime`-style deserializer. |
| **SCH-15** | `ChocolateyConfig.tags` | drop dual deserializer, `Vec<String>` only (DEC-11). At nuspec-emit time, anodizer joins on space. |

**YAML impact**: re-check both YAMLs for any `flags: "..."` (build), `mode: "0644"` (source), `umask:` (nfpm), `header:` / `footer:` (changelog), `timeout:` (notarize), `tags:` (chocolatey — already a list). Migrate any string-form to the new shape.

### W5.2 — Type-constraint batch

| SCH | Change |
|---|---|
| **SCH-27** | `binary_signs[].artifacts` — constrain to enum `{binary, none}`. Surface as separate type or `#[serde(rename_all = "snake_case")] enum BinarySignArtifacts`. |
| **SCH-31** | `NotarizeConfig.macos_native.use_` — constrain to enum `{notarytool}`. Match GR's current accepted set. |

### W5.3 — Field-add batch

| SCH | Change |
|---|---|
| **SCH-12** | `SrpmConfig` add fields: `bins`, `import_path`, `prefixes`, `build_host`, `pretrans`, `posttrans`, `prerelease`, `version_metadata`. Match GR `NFPM`/`Srpm`. Wire in `crates/stage-srpm`. |
| **SCH-17** | `AurSourceConfig` add `amd64_variant: Option<String>`. Equivalent to GR `Goamd64`. Wire as filter on which artifacts attach to AUR source pkg. |
| **SCH-24** | Per-entry `Authors` / `Logins` template field — already global; expose per-changelog-entry. Match GR. Touch `crates/stage-changelog`. |

### W5.4 — DRY-merge batch

| SCH | Change |
|---|---|
| **SCH-8** | `NfpmContent` and `NfpmContentConfig` — merge into one `NfpmContent`. Update both call sites. Match GR (one type). |
| **SCH-9** | `NfpmSignatureConfig` and `SrpmSignatureConfig` — **verify field divergence first**. `grep -A20 "struct NfpmSignatureConfig\|struct SrpmSignatureConfig"` on both. If GR has converged on one, merge to one. If GR keeps two, keep two and document the divergence. Default to merging if fields are identical. |

### W5.5 — Hard-break legacy-field batch (DEC-5)

| SCH | Change |
|---|---|
| **SCH-4** | `CrateConfig.docker` — drop. `docker_v2:` is canonical. |
| **SCH-13** | `HomebrewConfig.commit_author_name` + `commit_author_email` — drop. Use structured `commit_author: CommitAuthorConfig` only. |
| **SCH-16** | `AurConfig.url` — drop. `homepage:` + `url_template:` cover it. (GR has `URL` separately, but anodizer's `url` is a redundant alias not a distinct concept; verified by reading current AurConfig at `config.rs:2724`.) |
| **SCH-21** | Unify `TapConfig`, `BucketConfig`, `ChocolateyRepoConfig`, `WingetManifestsRepoConfig`, `KrewManifestsRepoConfig` → `RepositoryConfig`. Folds SCH-14 + SCH-18. Drop the 5 legacy types. Each publisher carries `repository: Option<RepositoryConfig>` (already wired); remove the parallel legacy field. Krew's two legacy fields (`manifests_repo` + `upstream_repo`) both die. |
| **SCH-30** | `NotarizeConfig` — drop top-level `disable` + per-cfg `enabled` doubled surface. Keep canonical `skip:` (DEC-6, post-WAVE 1) only. |
| **DEC-12** | `DockerV2Config.skip_push` — drop. Use canonical `skip:` to suppress publish. |
| **DEC-13** | `SnapcraftConfig.slots` (top-level) — drop. App-scoped slots remain via `apps.<name>.slots`. |

### W5.6 — Alias batch (selective)

DEC-8 says "no GR-config migration aliases." But these are anodizer-internal historical drift, not GR-migration sugar:

| SCH | Change |
|---|---|
| **SCH-5** | Top-level keys `nfpms` / `dmg` / `msi` / `flatpak` — verify against current top-level shape. Match GR canonical pluralization (`nfpms`, `dmgs`, `msis`, `flatpaks`). If anodizer already uses these canonical names, this is a no-op verification commit. |
| **SCH-11** | `MakeselfConfig.filename` — match GR field name. If already `filename`, no-op. |
| **SCH-34** | `AnnounceConfig.email` — **add `#[serde(alias = "smtp")]`**. This one IS a GR-internal migration (GR itself renamed `smtp:` → `email:` in v1.21+ with the alias). Mirroring GR's own alias is consistent with "use what GR uses today" since GR keeps both. |

### W5.7 — Behavior-toggle batch (single item)

| SCH | Change |
|---|---|
| **SCH-26** | `ChangelogConfig.snapshot: Option<bool>` — anodizer-original opt-in to render changelog during snapshot mode. **Verify** GR behavior first (`grep -n "Snapshot" /opt/repos/goreleaser/internal/pipe/changelog/changelog.go`). If GR skips changelog on snapshot (last verified: `if ctx.Snapshot { return true, nil }` at line ~62), then anodizer's opt-in deviates intentionally — keep the field, document the deviation in the type-doc comment with one line. **Cross-link**: this row was mis-bucketed in audit 4 as schema; the *behavior* (snapshot-skip default) is a Session C concern. The schema field (the toggle existing at all) is what lands here. |

---

## WAVE 6 — Both-YAML migration (ITEM-5)

**Decisions consumed**: all of WAVE 1–5.

This wave is mechanical: rewrite `/opt/repos/anodizer/.anodizer.yaml` and `/opt/repos/cfgd/.anodizer.yaml` to use the final schema. **Each YAML must parse, validate, and dry-run cleanly** before commit.

### W6.1 — anodizer YAML

- Add `defaults:` block with the high-repetition publisher config (`publish.cargo`, common archive shape, common sign config). 16 crate entries shrink as fields move to defaults.
- Apply DEC-9 inheritance — `{}` for crates that want full inheritance, override only what's specific.
- Verify all renames: no surviving `disable:`, `crates:` (publisher key), legacy struct fields.

### W6.2 — cfgd YAML

- Same migration. cfgd uses `workspaces:` (4 of them); `defaults.workspaces:` becomes the high-repetition home for nfpm, snapcraft, docker_v2 fields shared across cfgd / cfgd-operator / cfgd-csi.
- Verify all renames.

### W6.3 — Validation pass

```bash
$ task lint                                          # full anodizer pipeline
$ cd /opt/repos/cfgd && task lint                    # cfgd pipeline
$ task release-snapshot -- --config .anodizer.yaml   # both
```

### W6.4 — Documentation

- `xtask gen-docs` regenerates `docs/config-schema.md` (or equivalent) — must be in the same commit as the type changes (lint precondition).
- Update `docs/migration/v0.x-to-v0.y.md` (or create) with the hard-break list. Even though anodizer + cfgd are the only consumers, the migration doc serves as the audit trail and feeds the changelog.

---

## Out-of-scope (cross-linked)

| Item | Owner |
|---|---|
| `DockerV2.SBOM` default flip | Session C3 (behavior, not schema). |
| `Changelog.snapshot` default *behavior* (always-on vs opt-in) | Session C (the schema field lands here in W5.7; the default value is a behavior call). |
| Per-publisher CLI skip flags as a CLI-UX redesign (richer flag matrix, per-tap targeting, etc.) | Future CLI session. FOLL-1 only adds the basic per-publisher names matching the renamed publisher keys. |

---

## Test posture (per wave)

Each wave commit must satisfy `task lint` (which gates `task commit`). That covers fmt + clippy + cargo test + xtask gen-docs + dry-run-release.

Per-wave additional verifications:

| Wave | Extra verification |
|---|---|
| W1 | Snapshot tests for parsing both YAMLs (no `disable:` / no `env: HashMap` survivors). |
| W2 | New `defaults_merge_tests.rs` covers the 6 behaviors in W2.2. |
| W3 | CLI integration test: `--skip=cargo` accepted, `--skip=crates` rejected. |
| W4 | Parse-test: a YAML using the unified `HomebrewCaskConfig` at both top-level and per-crate. |
| W5 | Per-batch unit tests for the new types/aliases. |
| W6 | Both real YAMLs parse + validate + dry-run-release green. |

---

## What the implementation session does NOT do

- Does NOT push (always wait for explicit "push" / "ship").
- Does NOT touch Session C items (behaviors, defaults flips, per-publisher behavior changes).
- Does NOT add migration aliases for GR users (DEC-8).
- Does NOT preserve any legacy field as "compat shim" (DEC-5).
- Does NOT design new CLI flags beyond FOLL-1's per-publisher `--skip=` set.
- Does NOT introduce new publisher types or surfaces beyond what's in the inventory.

When done with WAVE 6: report back with a summary (waves landed, commits, test count delta), and wait for the user's "push" instruction before any `git push`.
