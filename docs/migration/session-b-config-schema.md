# Session B config-schema migration (2026-04-27)

Anodizer's only consumers are `/opt/repos/anodizer/.anodizer.yaml` and
`/opt/repos/cfgd/.anodizer.yaml`. Per **DEC-5 hard-break**, this wave makes
the schema renames listed below without deprecation aliases or shim warnings.
Both YAMLs were rewritten in a single landing per repo; rebase any in-flight
config edits on top of the new schema.

Audience: future-me (and any new consumer onboarding the schema). The
authoritative spec lives in
`.claude/audits/2026-04-config-gaps/_session-b-plan.md`.

---

## Hard-break renames (WAVE 1)

| Before | After | Notes |
|---|---|---|
| `disable: true` (24 sites) | `skip: true` | Per DEC-6. Affects every block carrying a `disable` field: flatpaks, app_bundles, dmgs, pkgs, nfpms, snapcrafts, dockers_v2, homebrews, scoops, chocolateys, wingets, aurs, nix, krews, notarize, sbom, signs, binary_signs, docker_signs, changelog, cloudsmiths, uploads, srpms, makeselves. |
| `env: { KEY: VAL }` (8 sites) | `env: ["KEY=VAL"]` | Per DEC-7. The HashMap form parsed two-way before; only the list-of-strings form parses now. Per-target nested maps (`overrides[].env`) are unchanged — different shape, different concept. |

## Cargo publisher rename (WAVE 3)

| Before | After |
|---|---|
| `publish: { crates: { enabled: true } }` | `publish: { cargo: {} }` |
| `publish: { crates: true }` (shorthand) | `publish: { cargo: {} }` |

The `crates` publisher key was renamed to `cargo` (matches the `cargo`
binary). The `enabled: true` shorthand was dropped — presence-as-opt-in is
the rule (`cargo: {}` opts in; `cargo: { skip: true }` opts out). The
`CargoPublishConfig` struct expanded to expose every field `cargo publish`
accepts that's config-file material (registry, no_verify, allow_dirty,
features, all_features, no_default_features, target, target_dir, jobs,
keep_going, manifest_path, locked, offline, frozen, index_timeout, skip).

CLI flag rename: `--skip=crates` → `--skip=cargo`.

## Publisher repository unification (WAVE 5.5)

| Before | After |
|---|---|
| `homebrew: { tap: { owner, name } }` | `homebrew: { repository: { owner, name } }` |
| `scoop: { bucket: { owner, name } }` | `scoop: { repository: { owner, name } }` |
| `krew: { manifests_repo: { owner, name } }` | `krew: { repository: { owner, name } }` |
| `homebrew: { commit_author_name, commit_author_email }` | `homebrew: { commit_author: { name, email } }` |
| `scoop: { commit_author_name, commit_author_email }` | `scoop: { commit_author: { name, email } }` |

## Docker rename (WAVE 5.5)

| Before | After |
|---|---|
| top-level `docker:` | `docker_v2:` |
| `docker_v2[].skip_push: true` | `docker_v2[].skip: true` |

## Snapcraft (WAVE 5.5)

| Before | After |
|---|---|
| top-level `snapcrafts[].slots` | `snapcrafts[].apps.<name>.slots` |

## Snapshot (deprecated alias, opportunistically migrated this wave)

| Before | After |
|---|---|
| `snapshot: { name_template: "..." }` | `snapshot: { version_template: "..." }` |

`name_template` still parses via `#[serde(alias = "name_template")]`
deprecation; the dogfood YAML now uses the canonical `version_template` to
silence the deprecation warning at `anodizer check` time.

---

## DEC-9 inheritance (the new defaults block)

Both YAMLs now hoist shared publisher / archive / sign config to a top-level
`defaults:` block. Per-crate entries inherit the defaults via three rules:

1. **Empty map at per-crate position (`{}`)** → inherit defaults verbatim.
2. **Per-crate value present** → deep-merge defaults under the per-crate
   value (per-crate wins on conflict).
3. **Per-crate `skip: true`** → suppress inheritance entirely (block is
   disabled, defaults do not leak in).

Anodizer's `.anodizer.yaml` example: every stage crate inherits the cargo
publisher slot from `defaults.publish.cargo: {}`, which removes 24
duplicate `publish: { cargo: {} }` blocks from the per-crate entries.
The CLI crate keeps its own `publish:` block because it ships
`homebrew`/`scoop`/`chocolatey`/`winget`/`aur`/`nix`/`krew` publishers that
no other crate uses.

For list-typed defaults (archives, nfpms, snapcrafts, dmgs, pkgs, msis,
nsis, app_bundles, flatpaks, docker_v2): the defaults entry deep-merges
into the per-crate entry that shares its identity key (`format` for
archives, `id`/`name`/`package_name` for packagers). Per-crate entries
without a matching identity stand alone; defaults entries without a match
are appended.

---

## Verification gate

```bash
$ task lint                                                            # full pipeline
$ cargo run --bin anodizer -- check --config .anodizer.yaml            # parse + validate
$ cargo run --bin anodizer -- release --snapshot --single-target --clean --dry-run
```

The snapshot dry-run is the actual gate — it must complete end-to-end with
the migrated YAML. `task lint` chains fmt → clippy → release build → docs →
snapshot dry-run, and is the precondition `task commit` enforces.
