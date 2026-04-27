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
into the per-crate entry that shares its identity key (`formats[0]` for
archives, `id`/`name`/`package_name` for packagers). Per-crate entries
without a matching identity stand alone; defaults entries without a match
are appended.

---

## Hard-break alias removal (post-WAVE 6 cleanup)

DEC-5 says "no aliases, no deprecation shims." Below is the full set of
serde aliases removed in this batch. Configs that still use the left-hand
spelling will fail to parse with `unknown field` from serde — rewrite them
to the right-hand spelling before upgrading.

### Top-level field aliases dropped

| Before (alias) | After (canonical) | Container |
|---|---|---|
| `sign:` | `signs:` | `Config`, `WorkspaceConfig` |
| `binary_sign:` | `binary_signs:` | `Config`, `WorkspaceConfig` |
| `sbom:` | `sboms:` | `Config` |
| `makeself:` | `makeselfs:` | `Config` |

### Field renames inside structs

| Struct | Before (alias) | After (canonical) |
|---|---|---|
| `BuildHooksConfig` | `before:` / `after:` | `pre:` / `post:` |
| `ArchiveHooksConfig` | `pre:` / `post:` | `before:` / `after:` |
| `ArchiveConfig` | `format: tar.gz` | `formats: [tar.gz]` (singular `format` field deleted) |
| `ArchiveConfig` | `builds: [...]` | `ids: [...]` |
| `FormatOverride` | `goos: windows` | `os: windows` |
| `FormatOverride` | `format: zip` | `formats: [zip]` (singular `format` field deleted) |
| `ExtraFileSpec::Detailed` | `name: "..."` | `name_template: "..."` |
| `MakeselfConfig` | `name_template: "..."` | `filename: "..."` (per SCH-11; field renamed) |
| `MakeselfFile` | `src:` / `dst:` | `source:` / `destination:` |
| `SnapshotConfig` | `name_template: "..."` | `version_template: "..."` |
| `EmailAnnounce` | `body_template:` | `message_template:` |
| `HomebrewConfig` (and Scoop / Chocolatey / Winget / Aur / AurSource / Nix / Krew) | `goamd64:` / `goarm:` | `amd64_variant:` / `arm_variant:` |
| `AurConfig` | `package_name:` | `name:` |
| `AurConfig` | `install_template:` | `package:` |
| `AurSourceConfig` | `package_name:` | `name:` |
| `AurSourceConfig` | `goamd64:` | `amd64_variant:` |
| `NfpmConfig` | `builds: [...]` | `ids: [...]` |
| `NfpmContent` | `source:` / `destination:` | `src:` / `dst:` |
| `NfpmSignatureConfig` | `passphrase:` | `key_passphrase:` |
| `SnapcraftConfig` | `builds: [...]` | `ids: [...]` |

### Aliases intentionally KEPT

| Alias | Canonical | Rationale |
|---|---|---|
| `announce.smtp:` | `announce.email:` | GoReleaser keeps both — anodizer mirrors so configs copied from GR docs parse without rewrites (SCH-34). |
| Snapcraft hyphen-aliases (`stop-mode`, `restart-condition`, `bus-name`, …) | underscore-form Rust field names | The hyphen form is snap.yaml's canonical spelling; the underscore form is needed because Rust identifiers can't contain hyphens. Both keep parsing because users write hyphens in snap.yaml. |

### Naming hazards (look near-identical, are not interchangeable)

`NfpmContent` and `MakeselfFile` both describe "a file to copy into the
package," but they use different key names because each mirrors its own
upstream tool's config:

| Struct | Source key | Destination key | Mirrors |
|---|---|---|---|
| `NfpmContent` | `src:` | `dst:` | nfpm.yaml |
| `MakeselfFile` | `source:` | `destination:` | makeself published spec |

Each struct rejects the other's key names with a serde "unknown field"
error and **does not suggest the cross-tool spelling** in the error
message. If you copy a content block from one publisher to the other,
re-key it by hand:

```yaml
# nfpms[].contents[]
- src: target/release/myapp
  dst: /usr/bin/myapp

# makeselfs[].files[]
- source: target/release/myapp
  destination: /opt/myapp/bin/myapp
```

### Deprecation infrastructure removed

`detect_deprecated_aliases`, `load_config_with_deprecations`, the
`Context.deprecated` field, the `Context.deprecate()` method, the per-key
detection branches and their unit tests, and the `DEPRECATED:` print in
`anodizer check` were all deleted. The CLI is now strictly
"parse → validate → run"; there is no warning surface for legacy keys.
Unknown fields error from serde directly with the list of accepted
spellings (no migration hint).

### Other cleanups bundled in this batch

- `resolve_repo_owner_name(publisher, legacy_field, modern, legacy_owner, legacy_name)` →
  `resolve_repo_owner_name(modern)`. The legacy-field bail path
  (modern + legacy both set with full owner/name pairs → error) was dead
  surface — every call site already passed `None, None` for the legacy
  pair. The `publisher: &str` param survived the first cleanup but was
  also dead (every call site passed a string-literal that never reached
  any branch); dropped in the post-DEC-5 cleanup along with the
  `Result<>` wrapper (the function never returned `Err`).
- `resolve_commit_opts(commit_author, legacy_name, legacy_email)` →
  `resolve_commit_opts(commit_author)`. Same story — every call site
  passed `None, None` for the legacy pair.
- `Format` template var on archive hooks now derives from `formats[0]`
  (or the global default) instead of the deleted `archive.format` field.
- `defaults.archives` identity key now uses `formats[0]` instead of `format`
  for the archive-merge engine.

## Verification gate

```bash
$ task lint                                                            # full pipeline
$ cargo run --bin anodizer -- check --config .anodizer.yaml            # parse + validate
$ cargo run --bin anodizer -- release --snapshot --single-target --clean --dry-run
```

The snapshot dry-run is the actual gate — it must complete end-to-end with
the migrated YAML. `task lint` chains fmt → clippy → release build → docs →
snapshot dry-run, and is the precondition `task commit` enforces.
