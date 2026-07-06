# Known bugs

A working list of bugs and findings collected *during* a large task so flow
isn't broken every time one surfaces — note it here, keep going, then circle
back and fix every item before the session ends. This is NOT a backlog, NOT a
TODO list, and NOT permission to ship anything unfixed. Every unchecked item
must be drained before the session is declared done; nothing here gets to
survive to a later session by default.

Each entry records what was found and the evidence, so the fix can resume
cold without re-investigating.

## Open

- [ ] **cargo poison guard blocks legitimate same-version re-cuts: cksum
  compare includes `.cargo_vcs_info.json` (v0.15.0 candidate — blocks cfgd
  v0.5.0).** `stage-publish/src/cargo.rs` `decide_already_published` skips an
  already-published version ONLY on byte-identical `.crate` sha256 vs the
  index cksum, hard-failing otherwise ("bump the version"). But cargo embeds
  the release commit in `.cargo_vcs_info.json` (`git.sha1`), so ANY re-cut of
  a partially-published release — new commit, identical crate sources — has a
  different tarball cksum and hard-fails the whole publish leg. Proven
  first-hand on cfgd 2026-07-05: cfgd-crd@0.5.0 published from `fb7e0c36`
  (index cksum `0844b18c…`); repackaging at that exact commit in a clean
  worktree reproduces `0844b18c…` byte-for-byte, while packaging at
  `53320c53` (cfgd-crd sources UNTOUCHED) yields `e4dc36b0…` — the only delta
  is the vcs sha1. Fix shape: compare content identity modulo
  `.cargo_vcs_info.json` (e.g. per-entry digests excluding the stamp, or
  normalize the stamp before hashing) so a true content poison still fails
  while a same-source re-cut skips clean. Until fixed, every monorepo re-cut
  after a partial publish is wedged on its already-published crates.
- [ ] **schemastore publisher misfires on per-crate publish-only legs whose
  crate doesn't own the schemas.** cfgd run 28755784707 `Publish cfgd-core`
  leg: `publisher schemastore failed: publish: crate 'cfgd' selected for a
  per-crate emission is not present in the crate universe` — the leg's
  universe is only `cfgd-core`, but the schemastore config targets `cfgd`.
  Non-required so it only warned, but it reports a FAILED publisher on every
  lib-crate leg (noise that can mask real failures). Fix shape: treat a
  schemastore selection whose crate is outside the leg's universe as
  not-applicable (skip), not failed.

_(Otherwise: no open code/config gaps. Every non-paid dogfooding field is landed in
`.anodizer.yaml` and committed; `flatpaks` is additionally PROVEN locally
(anodizer's own emitter produced a real 12.3 MB `.flatpak`). What remains before
a release is external SETUP only — see "Before-release setup" below. The two
`skip: true` blocks left in `.anodizer.yaml` are PAID-only (`notarize`,
`artifactories`).)_

## Before-release setup (firing prerequisites — user action)

Each active publisher's CONFIG is committed and reviewed; anodizer-side work is
DONE. Several FAIL at release until their backing account/secret/endpoint exists.
`[me]` = done this session; `[you]` = needs your action.

| Field (config landed) | Fires at release once… | Owner |
|---|---|---|
| `dockers_v2` → `registry.jarvispro.io/anodizer` | runner does `docker login registry.jarvispro.io` (creds in ns `jarvispro` `registry-credentials`); confirm ambient auth or add a login step | [you] |
| `npms` (scope `@tj-smith47`, tokenless OIDC — CODE done) | npmjs.com Trusted Publisher set (org `tj-smith47`, repo `anodizer`, workflow `release.yml`); one token-seeded publish creates `@tj-smith47/anodizer`, then drop the token; runner npm ≥ 11.5.1 + Node ≥ 22.14 | [you] |
| `dockerhub` README sync (`tsmthtj/anodizer`, required:false) | free Docker Hub repo `tsmthtj/anodizer` exists | secrets `DOCKER_USERNAME`+`DOCKER_PASSWORD` **[me]**; repo **[you]** |
| `gemfury` (account `tj-smith47`, **required:true**) | free fury.io OSS account + push-token GH secret | [you] — **hard-fails the whole release until set** |
| `uploads` → `https://uploads.jarvispro.io/anodizer/{Version}/` (PUT, real-tag only) | endpoint stood up + `UPLOAD_JARVISPRO_USERNAME`/`UPLOAD_JARVISPRO_SECRET` secrets | [you] |
| `publishers` → `minio-mirror` (`mc cp` to `jarvispro/anodizer-releases`) | `mc` alias `jarvispro` configured on the runner (`~/.mc/config.json`) | [you] |
| `flatpaks` (PROVEN locally) | confirm `flatpak-builder`+`bwrap` run on the dind release runner (unprivileged userns works on this box) | [you] — verify in first CI run |
| `app_bundles`/`dmgs`/`pkgs`/`msis`/`nsis` (unsigned) | the anodizer-action installer commits (`67eef08`+`6a07310`) reach action `master` (push pre-approved) — they install wixl/pkgbuild/genisoimage/makensis on Linux CI | **[me]** (commit done; push pending) |
| `before_publish` integrity gate | nothing — runs `test -s` on checksum+archive artifacts inline | **[me]** |

PAID — user decides, NOT blocking an unsigned release: `notarize` + signed
`pkgs`/`dmgs` (Apple Developer Program, $99/yr; `MACOS_SIGN_P12`/`MACOS_NOTARY_*`),
signed `msis`/`nsis` (Windows code-signing cert), `artifactories` (JFrog). These
are the two remaining `skip: true` blocks in `.anodizer.yaml`.

N/A — no host exists in the ecosystem: `github_urls`/`gitlab_urls`/`gitea_urls`
(all on github.com), `force_token` (niche per-token-kind override).

DECISION (pitch, not a gap) — `monorepo` (GR tag-prefix multi-component model) is
an ALTERNATIVE to the `crates` (anodizer) and `workspaces` (cfgd) models, both
already exercised live. Exercising it literally needs a repo restructured for
tag-prefixed component tags; none of the three fits without contortion.
Recommendation: leave unexercised (the two sibling multi-component models cover
the behavioral surface) OR nominate a repo to restructure. Your call; not blocking.

## Resolved

- [x] **`release.ids` silently drops signature/certificate/SBOM uploads —
  RESOLVED 2026-06-11 (review-fix pass on fb7e5a16).** Found while building
  the verify-release signature-expectation derivation:
  `collect_release_upload_candidates` applied `matches_id_filter` to every
  candidate, and `Signature`/`Certificate`/`Sbom` artifacts carried no `id`
  metadata and were not in the always-pass list — any non-empty
  `release.ids` excluded ALL of them and the release shipped unsigned,
  silently. **Fixed:** derived artifacts now inherit their SUBJECT's verdict
  — the sign/SBOM stages record `subject_kind` plus the subject's build `id`
  on every Signature/Certificate/Sbom registration, and `matches_id_filter`
  judges that record (always-pass subject → pass; otherwise the inherited id
  must match; no record — project-wide `any` SBOMs, pre-fix metadata.json in
  merge mode — passes rather than silently dropping). The verify-release
  expectation derivation applies the same per-subject verdict (its previous
  blanket "no expectations when ids set" mirror removed in the same change).
  **Evidence:** core `artifact::tests::id_filter_*` (5 tests); upload-side
  `test_release_upload_candidates_ids_filter_signatures_inherit_subject_verdict`
  (subject-included sig+sbom upload, subject-excluded don't, checksum sigs
  always do); stage-sign `sign_registrations_carry_subject_provenance` +
  `release_ids_subject_verdict_filters_expectations`; verify-release
  `derived_expectations_follow_subject_verdict_under_release_ids`.

- [x] **Test-suite PATH race — RESOLVED 2026-06-11 (bc2e553e + review
  pass).** Originally: tests simulating missing tools
  replaced the process-global `PATH` with an empty tempdir under `env_mutex`,
  which serialises mutators only — concurrent spawn-via-PATH tests
  (schema_validation bash/xmllint/dpkg-deb/rpm, git/gh fixtures) hit the
  window and got `No such file or directory` (observed 2026-06-10 in aur
  workspace_lockstep, 2026-06-11 in chocolatey xmllint).
  **Fixed (bc2e553e + 2026-06-11 review pass):** all three wholesale
  empty-dir PATH replacements are gone — `preflight.rs`
  dry_run_spawn_failure (→ `run_cargo_dry_run_with_binary` seam), `cargo.rs`
  rollback_dry_run (→ prepended argv-recording stub + empty-log assertion),
  `stage-blob/kms.rs` preflight_errors_when_cli_missing
  (→ `preflight_kms_cli_with_binary` seam). No test can blind another's
  spawn anymore; the observed ENOENT flakes are unreproducible by
  construction. `npm/tests.rs` assessed: prepend-style (stub dir + original
  PATH tail) inside `unsafe` under `env_mutex` — the sanctioned pattern, not
  a clobber.
  **Also fixed (same review pass):** the cross-group same-tool shadowing
  hole — `cargo.rs` / `lib.rs` cargo-stub prepend tests relied on
  `#[serial(cargo_stub_path)]` alone, a DIFFERENT serial_test group from
  the unnamed-`#[serial]` fake-cargo dry-run tests in `preflight.rs`, so
  their PATH windows could overlap. Every prepend mutator (`with_path`,
  the inline `install_cargo_stub` sites, `lib.rs` dispatch-rollback) now
  also holds `env_mutex` across mutate+spawn+restore, joining
  `fake_tool::activate` and `npm/tests.rs` on the one canonical lock.
  Nothing actionable remains; residual exposure is drift-only (a FUTURE
  test that prepends a stub or spawns a stubbed tool without taking
  `env_mutex` reopens the window — `fake_tool::activate`'s doc carries the
  requirement).
  Moved to Resolved 2026-06-11.


- [x] **`if:` boolean context vars are injected as strings, so `not IsSnapshot` /
  bare `{% if IsSnapshot %}` silently misbehave (GoReleaser-migration footgun).** A
  user-written `if: "{{ not .IsSnapshot }}"` renders `"false"` in EVERY mode — snapshot
  *and* release — and the if-engine skips the stage with no warning. Root cause:
  `IsSnapshot` / `IsNightly` / `IsHarness` / `IsDraft` / `NightlyBuild` are set via
  `TemplateVars::set` (the string-only `vars: HashMap<String,String>`) in `Context`'s
  var-injection (grep `set("IsHarness"` in `core/src/context.rs`) and in `core/src/hooks.rs`
  (grep `set("IsSnapshot"`). Tera treats any non-empty
  string — `"true"` AND `"false"` — as truthy, so `not "false"` → `false` → renders
  `"false"` → skipped. This is NOT unavoidable Tera behavior: GoReleaser's Go templates
  expose `.IsSnapshot` as a real bool where `{{ if .IsSnapshot }}` / `{{ not .IsSnapshot }}`
  work, so a migrant writing `not .IsSnapshot` writes idiomatic code that fails silently.
  Our own `.anodizer.yaml` and cfgd's only work because they use the explicit-compare
  workaround `{% if IsSnapshot == "false" or IsHarness == "true" %}`.
  **Fix:** inject these via the existing typed channel `TemplateVars::set_structured`
  (`TemplateVars::set_structured` in `core/src/template/vars.rs`, already merged into the
  Tera context as-is) as
  `tera::Value::Bool` instead of `set`. Tera still renders `Value::Bool` as `"true"`/`"false"`
  in interpolation, so `{{ IsSnapshot }}` and the if-engine's `"false"`-string falsy check
  keep working, while `not` / `if` / `and` / `or` become correct.
  **Scope:** a key can't live in both the string and structured maps (collision) — go
  structured-only and update the internal string readers, which are all TESTS (grep
  `get("IsSnapshot")` / `get("IsDraft")` / `get("IsNightly")` / `get("NightlyBuild")` in
  the `core/src/context.rs` test module and `core/src/test_helpers/mod.rs` — re-point to
  `get_structured`). No production code reads these via `.get()`.
  **Consider also:** a strict-mode lint that hard-errors when an `if:` references a known-bool
  var with bare-truthiness or `not`, so the silent-skip becomes loud for the next user.
  **Migration hazard (must handle in the same change):** once these are real bools,
  `IsSnapshot == "false"` (string compare) stops matching — Tera does not coerce `Bool` ↔
  `str` — which silently RE-skips every stage using today's explicit-compare workaround,
  including our own `.anodizer.yaml` and cfgd's sign stages. The fix must rewrite all
  workaround sites to the natural `not IsSnapshot` form in the same release (and call the
  break out in the changelog), or preserve string-compare equivalence; do not ship the bool
  change alone.
  **Found:** cfgd dogfooding audit 2026-06-10 — cfgd would have shipped unsigned releases
  because all five sign-stage `if:` used the broken form.
  **Resolved 2026-06-11** in `fix(core): inject Is* template vars as typed bools` — all
  Is\* flags + NightlyBuild now `set_structured` (Bool/Number), `.anodizer.yaml` rewritten
  to `{{ not IsSnapshot or IsHarness }}`, stale string-compares hard-error in
  `evaluate_if_condition`/`try_evaluates_to_true`. Investigation inverted the root cause:
  the `"true"/"false"` coercion in `build_tera_context` already made `not IsSnapshot`
  work; the explicit-compare "workaround" was the broken form (Tera `Bool == str` never
  matches) — confirmed live: v0.8.0 shipped with zero signature assets. cfgd's 5 sign-stage
  sites (grep `IsSnapshot == "false"` in cfgd/.anodizer.yaml) still need the
  consumer-side migration.


  AUR SSH key EEXIST on retry.** On retry after a failed AUR publish, the file already existed
  from the prior run and `write_ssh_key_secure` (which opens with `O_CREAT|O_EXCL`) failed with
  EEXIST (os error 17). User had to manually `rm /tmp/.anodizer_ssh_key` to retry. FIX: use a
  per-invocation unique path (e.g. `tempfile::NamedTempFile` or `tempfile::tempdir()` → `key_file`)
  so concurrent runs don't collide and stale files never block a retry. **FIXED:**
  `6e2b2387 fix(aur): unique SSH key filename per clone to prevent EEXIST on retry` — now uses
  `tempfile::tempdir()` to generate a unique per-invocation key path, eliminating the collision.
  Source: `crates/stage-publish/src/util/clone.rs` (`write_ssh_key_secure`).
  Surfaced: v0.7.0 local AUR push, 2026-06-10. Fixed: 2026-06-10.

  ArtifactName not populated for url_template users.** The template `{{ ArtifactName }}`
  (the natural way to reference the archive filename in a download URL template) silently
  expanded to empty string because `aur_build_sources` did not populate it via `render_url_template_with_ctx`.
  `arch` was the PKGBUILD arch (`x86_64`/`aarch64`), not the anodizer archive arch (`amd64`/`arm64`),
  compounding the mismatch. FIX: pass `a.path.file_name().to_string_lossy()` as the `name` parameter,
  which sets `ArtifactName` correctly and makes the natural template work. **FIXED:**
  `c4031cd1 fix(aur): set ArtifactName from archive filename in url_template render` — now correctly
  populates `ArtifactName` from the archive filename before template render. Source:
  `crates/stage-publish/src/aur.rs` (`aur_build_sources`, ~line 672). Surfaced: v0.7.0 local AUR push,
  2026-06-10. Fixed: 2026-06-10.

  AUR missing actionable error when metadata["url"] absent.** For publish-only runs from a pre-upload
  local dist the artifact `url` metadata was not populated (it is set by the release-upload stage),
  so the PKGBUILD `source=()` received local paths the AUR server cannot validate, producing a
  misleading "missing source file: /path/to/dist/…" error rather than an actionable message. FIX:
  when `metadata["url"]` is absent AND no `url_template` is configured, fail with an actionable error
  explaining that the dist was built without asset upload (or that `aur.url_template` must be set).
  **FIXED:** `532096ab fix(publish): error loudly when artifact URL absent in publish mode; tolerate in snapshot`
  — now detects the condition early and returns an actionable error. Source:
  `crates/stage-publish/src/util/artifacts.rs` (`artifact_to_os_artifact`, ~line 87-91). Surfaced:
  v0.7.0 local AUR push, 2026-06-10. Fixed: 2026-06-10.

  MCP registry template not rendered.** The `registry` field in MCP config carries a template string
  (e.g. `"{{ .Env.MCP_REGISTRY }}"`) but was passed directly to the HTTP client without rendering,
  so the unresolved template sent as a literal URL. FIX: call `ctx.render_template(resolve_registry_url(...))` 
  at the call site so `registry: "{{ .Env.MCP_REGISTRY }}"` resolves before being used in the HTTP POST.
  **FIXED:** `mcp/mod.rs` now contains `ctx.render_template(resolve_registry_url(...))`, correctly
  rendering the registry URL before use. Source: `crates/stage-publish/src/publishers/mcp/mod.rs`.
  Surfaced: 2026-06-06. Fixed: 2026-06-10.

  **CommitOptions.signing fields not rendered.** `CommitOptions.signing` changed from `Option<&CommitSigningConfig>`
  (raw reference) to `Option<CommitSigningConfig>` (owned), but the signing config fields (gpg_sign, key_id, etc.)
  carry template strings and were not being rendered before use in `git -c` args. FIX:
  `resolve_commit_opts` now renders each field via `render_or_warn` before storing in the owned struct, so
  `git -c user.signingkey=…` receives the resolved value. **FIXED:** `commit.rs` now renders all
  signing fields (key_id, gpg_program, etc.) via `render_or_warn` before storing them in `CommitSigningConfig`.
  Source: `crates/stage-publish/src/publishers/commit.rs`. Surfaced: 2026-06-06. Fixed: 2026-06-10.



  **Snapshot dispatches live GitHub release backend (receives "release: no GitHub token") instead of treating snapshot as non-publishing.** Fix: `crates/stage-release/src/run.rs`
  now computes `let dry_run = ctx.is_dry_run() || ctx.is_snapshot();` so snapshot takes the
  "would create …" telemetry path. Regression test `test_snapshot_without_dry_run_does_not_reach_live_backend`
  (`crates/stage-release/src/tests.rs`), proven red→green (revert → FAILED exit 101; restore →
  pass; full `-p anodizer-stage-release` 433 passed; clippy clean). Single dispatch source, so
  gitlab/gitea inherit the fix. Surfaced dogfooding the cfgd
  schemastore plan (2026-06-06). Root cause: in `crates/stage-release/src/run.rs` the line
  `let dry_run = ctx.is_dry_run();` — does NOT OR-in `ctx.is_snapshot()`, so under `--snapshot`
  `release_one_crate` dispatches to the live backend (`github::run_github_backend`, which
  bails with `"release: no GitHub token available"` in `github/backend.rs` without a token —
  and WOULD create a real GitHub release if a token were present). The intended contract is the
  opposite: `run_github_backend`'s ID-capture step already guards with
  `!ctx.is_dry_run() && !ctx.is_snapshot()` (`github/publisher.rs`). All three SCM
  backends (github/gitlab/gitea) share the dispatch and the bug. Masked because anodizer's own
  determinism harness always runs `release --snapshot --skip=release,...` (self-test blindspot,
  cf. project_anodizer_self_determinism_blindspot). Repro: `anodizer release --workspace cfgd
  --snapshot --host-targets --clean` (without `--skip release`) → `Error: release: no GitHub
  token available`. Proposed fix: `let dry_run = ctx.is_dry_run() || ctx.is_snapshot();` (so
  snapshot takes the "(dry-run) would create …" telemetry path), + a regression test asserting
  `--snapshot` with no token does not reach the live backend. Verify gitlab/gitea parity.

  mismatch in the validator's re-render.** Surfaced in the cfgd dogfood (2026-06-06). Symptom was
  `nfpm: field 'deb:Version' — built …carries version Some("1:0.4.0~SNAPSHOT-b348321-1"),
  config resolved "0.4.0"`, so `release --snapshot` on any nfpm config hard-failed before the
  publish stage. ROOT CAUSE: the nfpm BUILD stage (`crates/stage-nfpm/src/run.rs`,
  `NfpmStage::run`) reads the global `Version` template var ONCE and stamps every crate's package
  `version:` from it — in snapshot the `<base>-SNAPSHOT-<sha>` value. The VALIDATOR
  (`crates/stage-publish/src/schema_validation/nfpm.rs`, `NfpmSchemaValidator::validate`)
  re-rendered the nfpm yaml inside `with_validated_crate_scope` →
  `anodizer_core::crate_scope::with_crate_scope`, which reset `Version` to the crate's TAG-derived
  bare value (`semver.version_string()` = `0.4.0`). So the validator rendered `0.4.0` while the
  build stamped `0.4.0-SNAPSHOT-<sha>` → control cross-check rejected every snapshot package.
  Why CI never caught it: the existing per-crate test drove the validator with
  `test_current_version_resolver` (returns the already-snapshot-labeled `ctx.version()`), and the
  other tests called `nfpm_yaml_configs_for_crate` directly — neither modeled production's
  `resolve_crate_tag`, which returns the BARE tag. FIX: new `render_build_matched_nfpm_configs`
  pins `Version`/`RawVersion` back to the captured global artifact version (`ctx.version()`,
  taken pre-scope) before the render — mirroring the binstall/nix `scope_artifact_version`
  cross-check — while keeping the per-crate name/tag scope for templated fields. Correct in all
  three config modes because each mode's build read the same global `Version` this captures.
  Regression tests (red→green proven by neutering the override → both rendered bare `0.4.0`/`1.2.0`):
  `validate_renders_build_artifact_version_not_bare_tag_rederivation` (lockstep snapshot, the cfgd
  repro) + `validate_per_crate_independent_renders_each_own_snapshot_version` (per-crate oracle).
  `anodizer-stage-publish` 1466 passed, clippy clean. The earlier adjacent
  `expected_control` prerelease/version_metadata fold
  (`expected_control_folds_prerelease_for_snapshot_versions`) stays — it covers a user-set
  `nfpm.prerelease:`, orthogonal to this snapshot-template case. PROVEN LIVE (2026-06-06): the cfgd
  capstone (`release --workspace cfgd --snapshot --host-targets --clean`, nfpm NOT skipped) ran
  `emission-validate` CLEAN with zero nfpm findings against the real built deb whose control
  `Version` is `1:0.4.0~SNAPSHOT-a0439ee-1` (the exact grammar that previously failed), then
  proceeded into before-publish and `[release] (dry-run) would create GitHub Release 'cfgd v0.4.0'
  (tag=v0.4.0-SNAPSHOT-a0439ee)` — which also re-confirms the Bug 1 snapshot-release fix live. The
  run's only remaining failures (makeself, appimage, upx, sbom/syft, sign/docker-sign cosign
  keyless) are this box's missing-tool/credential env limits (see [[project_prepush_not_local_runnable]]),
  not code — they are CI-only stages and were `--skip`ped to reach the publish-stage validation.

  release step — IN-REPO IMPLEMENTATION COMPLETE.** Built the `schemastore:` publisher
  (Manager group, sibling to krew/homebrew) end-to-end via the spec
  (`.claude/archive/2026-06-05-schemastore-publisher-spec.md`) + plan
  (`docs/superpowers/plans/2026-06-05-schemastore-publisher.md`), Tasks 1–17, each
  spec-reviewed + quality-reviewed. **Shape:** field-presence selects mode (`url` ⇒
  external catalog-entry-only; `schema_file` ⇒ vendor a file into
  `src/schemas/json/<slug>.json`); per-schema cascade (entry → block → derived); surgical
  byte-preserving `catalog.json` splice (insertion-ordered, prettier key-order);
  `versions` carry-forward for versioned vendor; auto-add to `highSchemaVersion` for
  draft-2019-09/2020-12 schemas (keyed on the vendored **filename** `<slug>.json` —
  matches SchemaStore CI `path.basename(schema.path)`, verified against
  `/opt/repos/schemastore/cli.js` `highSchemaVersion.includes(schema.name)`); fork-sync
  to upstream master; pending-PR
  idempotency; close-PR rollback; per-crate version scope via `with_published_crate_scope`
  (proven across single/lockstep/per-crate modes). External-URL unreachable is a Warning
  not a Blocker (anodizer may release the very site that hosts the schema). A final
  holistic review caught a release-breaking bug (allowlist keyed on `entry.name` not the
  vendored filename — cfgd-module is draft-2020-12, so its dogfood PR would have hard-failed
  CI) + an Important preflight gap (derived descriptions bypassed preflight sanitize),
  both fixed (`0d825976`). **Evidence:** full `task ci` green — 6904 tests, 0 failed
  (69 schemastore); clippy `-D warnings` clean; zola docs build 0 errors; gen-docs --check
  clean. Now on master (the `publisher-required-config` branch is superseded). The
  cross-repo cfgd dogfood was de-scoped by the user (2026-06-06).

  (test-isolation flake).** Every per-crate publisher's `run()` re-scopes each crate
  via `with_published_crate_scope` → `resolve_crate_tag`, which resolves the crate's
  version by running `git tag --list` against `project_root` — and `project_root`
  fell back to `PathBuf::from(".")` (the process cwd) when a test left it unset. The
  affected unit tests (16 across krew/aur/nix/homebrew/scoop/winget/chocolatey: the
  `*_dry_run_*`, `*_implicit_all_*`, `*_skip_upload_*`, `*_visible_work_contract`, and
  winget `*_records_target`/`*_empty_selection_publishes_all` cases) built their
  context with no `project_root`, so `resolve_crate_tag` read whatever git checkout the
  cwd happened to point at. A sibling test in the same binary (`mcp/tests.rs`, via
  `CwdGuard::new(temp_repo)`) swaps the process-wide cwd to a **tag-less** tempdir for
  the duration of its body; when a publisher test ran concurrently in that window,
  `resolve_crate_tag` returned `None`, `with_crate_scope` errored
  (`crate 'demo' … has no release tag matching its tag_template; cannot derive its
  version`), and the test's `.expect()` on `run()` panicked. The race was inverted from
  the original report: the test *needs* a resolvable tag and fails when the ambient cwd
  has none. **Fixed:** added `crate::testing::hermetic_tagged_repo()` (a `#[cfg(test)]`
  helper that `init_git_repo_with_commits`-seeds a throwaway repo tagged `v0.1.0`) and
  pointed each affected test's `project_root` at it, so version resolution reads a
  deterministic tag set and is immune to ambient cwd. **Evidence:** full
  `-p anodizer-stage-publish --lib` run single-threaded from a tag-less temp cwd went
  19-failed → 0-failed (1423 passed); 3× parallel full-suite runs all green
  (1423/0); v999-probe (create `v999.0.0` in the live repo, run the krew+aur
  implicit-all tests) passes, proving immunity to ambient `v*` tags; clippy
  `-p anodizer-stage-publish --all-targets -D warnings` clean.

  cwd-relative.** `release/mod.rs` `check_workspace_files_changed` →
  `git::paths_changed_since_tag(tag, ["Cargo.toml", "Cargo.lock"])` ran the
  pathspec relative to the current directory, so invoked from a subdirectory it
  inspected the subdir's manifests, not the workspace root's. The per-crate
  loop had the same coupling (`resolve_selected_crates` passed `cwd` into
  `detect_changed_crates`). **Fixed:** `resolve_selected_crates` now discovers
  the Cargo root via `discover_workspace_root(opts.config_override)` (the same
  unification `tag`/`changelog`/`bump` already share) and threads it through
  `detect_changed_crates` → `check_workspace_files_changed`, which now calls
  `git::paths_changed_since_tag_in(workspace_root, …)`. Under the bug, a
  per-crate `Cargo.toml` edit run from `crates/<x>/` false-promoted the entire
  workspace (the subdir's own manifest matched the bare `Cargo.toml` pathspec);
  under-detection was otherwise masked by the `--all` empty-means-all collapse.
  Regression test: `test_e2e_release_change_detection_from_subdir`
  (`crates/cli/tests/integration.rs`) — edits only `crates/myapp/Cargo.toml`,
  runs `release --all` from `crates/myapp/`, asserts only `myapp` is selected
  and `solo-lib`/`core-lib`/`helper-lib` are not. Proven red→green (stash-revert
  of the fix fails the test on `crate 'solo-lib' must NOT be selected`).
