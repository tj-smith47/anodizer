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

_(No open code/config gaps. Both v0.15.0 npm findings — the retry-budget
guillotine and the OIDC-fallback — are fixed and proven; see Resolved. Every
non-paid dogfooding field
is landed in `.anodizer.yaml` and committed; `flatpaks` is additionally PROVEN
locally (anodizer's own emitter produced a real 12.3 MB `.flatpak`). The two
`skip: true` blocks left in `.anodizer.yaml` are PAID-only (`notarize`,
`artifactories`).)_

## Before-release setup (firing prerequisites — user action)

Each active publisher's CONFIG is committed and reviewed; anodizer-side work is
DONE. Several FAIL at release until their backing account/secret/endpoint exists.
`[me]` = done this session; `[you]` = needs your action.

| Field (config landed) | Fires at release once… | Owner |
|---|---|---|
| `dockers_v2` → `registry.jarvispro.io/anodizer` | runner does `docker login registry.jarvispro.io` (creds in ns `jarvispro` `registry-credentials`); confirm ambient auth or add a login step | [you] |
| `npms` (scope `@tj-smith47`, tokenless OIDC) | ✅ PROVEN 2026-07-06 — Trusted Publisher live, packages created; v0.15.0 re-run published with real provenance (0 token fallbacks). Runner npm ≥ 11.5.1 floor enforced by anodizer-action `5629a41` | **[done]** |
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

- [x] **Chocolatey's prepublish guard hard-requires `xmllint` but no
  requirement ever declared it — release died AFTER signing + GitHub
  release (cfgd v0.5.0 run 28853272910, Publish cfgd leg, 2026-07-07
  15:27Z) — RESOLVED 2026-07-07.** The strict pre-publish schema floor
  treats a missing `xmllint` as a gate FAILURE (chocolatey moderation is a
  one-way door), but `ChocolateyPublisher::requirements()` declared only
  the CHOCOLATEY_API_KEY secret — so `anodizer tools` never told the
  Action to install xmllint, preflight never probed it, and ubuntu-latest
  (which ships without it) sailed through to the guard post-release.
  **Fixed** in `chocolatey/publisher.rs`: `requirements()` now emits
  `Tool{xmllint}` (hard, non-advisory) per active entry, gated by the same
  `entry_inactive` filter as the secret. Sweeping the whole
  spawn-vs-declared surface (every publisher + the schema_validation
  layer) surfaced and closed the rest of the class in the same change:
  (1) `git_repo_requirements` never demanded `ssh` on the
  `repository.git.url` SSH-clone path (git spawns ssh unless a custom
  `ssh_command` replaces it) — now hard-demanded, mirroring
  `aur_ssh_requirements`; (2) gracefully-degrading tools were invisible to
  the auto-install layer — added `Publisher::advisory_requirements()`
  (collected via `SourcedRequirement::new_advisory`, same frame as the
  build stage's cross toolchain) and declared `ruby` (homebrew formula+cask
  floor), `bash` (aur + upstream-aur PKGBUILD floor), `nix-instantiate`
  (nix floor), `gh` (preferred PR transport for homebrew/nix when
  `pull_request.enabled`, always for winget/schemastore; full REST-API
  fallback), and `dpkg-deb`/`rpm` (nfpm package cross-check, via
  `anodizer_stage_nfpm::advisory_env_requirements`). Advisory reqs ride
  the publish row's deselection predicate, so `--publishers`/`--skip`
  drop them with their publisher.
  **Evidence (red→green):** requirements-level
  `chocolatey_requirements_emit_xmllint_tool` failed pre-fix with
  `got: [EnvAllOf { vars: ["CHOCOLATEY_API_KEY"] }]`, tools-level
  `chocolatey_config_emits_xmllint_in_json` failed pre-fix; both green
  after. Class fixes: `git_repo_requirements_ssh_url_demands_ssh_tool`,
  9 per-publisher `*_advisory_requirements_*` tests,
  `advisory_env_requirements_track_configured_formats` (nfpm),
  `homebrew_config_emits_advisory_ruby_in_json` and
  `advisory_requirements_respect_publisher_deselection` — every one run
  red first, then green. Live-eyeballed `anodizer tools --json` against a
  chocolatey+homebrew+nfpm config: xmllint/git/cargo/nfpm required,
  ruby/dpkg-deb/rpm/zig recommended, stdout pure JSON.

- [x] **Parallel keyless cosign signing races on the cold TUF cache and
  fast-fail retries lose every round (cfgd v0.5.0 run 28853272910, Publish
  cfgd leg, 3×: 09:42Z + 10:05Z + retry 2026-07-07) — RESOLVED
  2026-07-07.** The sign stage fanned out `signing 22 artifacts with
  parallelism=4`; on a fresh runner all four first-wave workers invoked
  cosign simultaneously against an uninitialized `~/.sigstore` TUF store,
  and the losers died with `getting key from Fulcio: getting CTFE public
  keys: creating cached local store: resource temporarily unavailable`
  (flock EAGAIN — the known cosign concurrent TUF-init race). cosign's own
  3 internal tries burned out in ~2.5s, entirely inside the contention
  window; anodizer had NO retry of its own, so the stage aborted fail-fast
  with 0/22 signed. Sigstore infra was UP each time; a different artifact
  lost each run — pure timing. **Fixed** in
  `crates/stage-sign/src/process.rs`, both halves: (1) serial TUF warm-up —
  a keyless-cosign fan-out (`is_keyless_cosign`: basename `cosign`, no
  `--key`) signs its FIRST artifact alone, warming the trust root with a
  single process, then parallelizes the remainder; (2) `retry_transient` —
  cosign invocations (network-dependent: Fulcio/Rekor/TUF CDN) retry per
  `COSIGN_TRANSIENT_RETRY` (5 attempts, jittered exponential backoff 2s
  base / 15s cap, nominal 29s spread), spawn-`NotFound` fast-fails, and
  local signers (gpg/osslsigncode) keep the single fast attempt. Rider: the
  sign stage now honors `ctx.options.parallelism` like every other
  subprocess-per-job stage instead of deriving its own
  `available_parallelism()`.
  **Evidence (red→green: all three behavioral tests failed on pre-fix code
  with the exact production stderr — 6/8 stub workers lost an mkdir-lock
  TUF-init race — then passed after the fix):**
  `keyless_cosign_survives_cold_tuf_cache_with_parallel_fan_out`,
  `keyless_cosign_first_invocation_completes_before_fan_out` (pins the
  serialization: exactly one start precedes the earliest end),
  `transient_cosign_failure_is_retried_to_success`,
  `non_cosign_signer_fails_fast_without_retry`,
  `cosign_retry_backoff_spans_the_contention_window` (injected sleep pins
  the ±20% jitter envelope and the ≥15s nominal spread),
  `missing_binary_fast_fails_without_retry`,
  `success_on_first_attempt_never_sleeps`, `keyless_cosign_classifier`;
  stage-sign suite 174/174, cli suite 1114/1114 + all integration suites
  green.

- [x] **Cargo poison guard dead-ends every re-cut of a partially-published
  workspace release (cfgd v0.5.0 attempt #5, 2026-07-07) — RESOLVED
  2026-07-07.** cfgd-crd@0.5.0 was published to
  crates.io by a PRIOR release run; the re-cut's locally-packaged tarball
  differed from the published one in exactly two files —
  `cfgd-crd-0.5.0/CHANGELOG.md` (anodizer's own changelog stage regenerates
  it each re-cut) and `cfgd-crd-0.5.0/Cargo.lock` (workspace lockfile moved
  via an unrelated dependabot bump) — with byte-identical sources. The guard
  ("ALREADY published with DIFFERENT content… Differing entries:
  cfgd-crd-0.5.0/CHANGELOG.md, cfgd-crd-0.5.0/Cargo.lock") hard-failed, and
  since anodizer publishes workspace crates sequentially with fail-stop,
  partially-published releases are a state the tool itself guarantees — so
  every future re-cut was dead. **Root cause:** `crates_equal_modulo_vcs`'s
  equivalence set covered only `.cargo_vcs_info.json` (modulo `git.sha1`),
  treating the tool's own release-process artifacts as poison. **Fixed:**
  built-in (deliberately NOT user-configurable) re-cut equivalence rules in
  `crates/stage-publish/src/cargo.rs`, crate-root position only: crate-root
  `CHANGELOG.md` drift is equivalent ONLY when the changelog stage is
  configured/active for the run (`changelog_stage_regenerates_files`,
  threaded as an explicit bool from `publish_to_cargo_with_guard` through
  `decide_already_published`); crate-root `Cargo.lock` drift is equivalent
  ONLY for lib-only crates (bin-ness judged from the packaged normalized
  `Cargo.toml` inside the tarball + conventional bin-source paths — binary
  crates stay byte-strict because `cargo install --locked` exposes the
  packaged lockfile). Clean-skip log enumerates exactly which normalizations
  applied; non-qualifying drift hard-fails naming the file AND why it was
  not forgiven; nested files and `Cargo.toml` stay byte-strict.
  **Evidence (red→green proven by temporarily neutering the two new
  normalization branches — headline test then reproduced the attempt-#5
  hard-fail verbatim):**
  `decide_already_published_recut_changelog_and_lockfile_skips_with_changelog_stage`
  (the exact cfgd-crd@0.5.0 scenario),
  `decide_already_published_changelog_drift_without_changelog_stage_hard_fails`,
  `decide_already_published_lockfile_drift_on_binary_crate_hard_fails`,
  `crates_equal_modulo_vcs_source_drift_beside_normalizable_files_differs`,
  `crates_equal_modulo_vcs_nested_changelog_and_lockfile_are_byte_compared`,
  `crates_equal_modulo_vcs_cargo_toml_drift_always_differs`,
  `packaged_crate_has_bin_targets_reads_the_normalized_manifest`;
  stage-publish suite 2524/2524.

- [x] **Tag rollback consulted only the per-run summary, deleting tags whose
  versions were burned on crates.io by a PRIOR run (cfgd crd-v0.5.0, attempt
  #5) — RESOLVED 2026-07-07.** Attempt #5's
  rollback logged "no one-way-door publisher landed for crd-v0.5.0 (per run
  summary) — rollback permitted" and deleted the tag even though crd@0.5.0
  had been live on crates.io since a prior run. **Root cause:** the
  published-state guard's evidence layers (run summary; GH-release fallback)
  both answer per-run/per-checkout questions, but one-way-door burn state is
  GLOBAL. **Fixed:** `check_not_irreversibly_published`
  (`crates/cli/src/commands/tag/rollback.rs`) gained a layer-2 crates.io
  sparse-index probe: each deletable tag maps to `(crate, version)` pairs via
  the config's crate tag families (`crates_io_versions_for_tag`; per-crate
  AND lockstep tags, custom-registry crates excluded), and the probe reuses
  the publish stage's sparse-index client
  (`anodizer_stage_publish::cargo::published_on_crates_io`, new pub wrapper
  over `is_already_published`; `targets_crates_io` made pub). Version live on
  the index → refuse (fix forward); index unreachable → refuse (fail
  closed); no parseable config → warn + proceed (no mapping to probe).
  `--force` help (cli + rustdoc) documents the new layer; docs
  (`release-resilience.md`) updated. Test seam: injectable
  `index_probe: &dyn Fn(&str, &str) -> Result<bool>` param (same convention
  as the `gh_binary` stub), plus a harness-gated
  `ANODIZER_TEST_CRATES_IO_INDEX_BASE` env override (honored only under
  `ANODIZE_TEST_HARNESS=1`) so the spawned-binary failure-policy integration
  test stays hermetic (`test-project@0.1.0` is squatted on the real index).
  **Evidence (red→green proven by temporarily removing the layer-2 call —
  refuse + fail-closed tests then failed):**
  `crates_io_probe_refuses_burned_version_despite_clean_summary` (asserts the
  probe receives `cfgd-crd@0.5.0` from tag `crd-v0.5.0`),
  `crates_io_probe_permits_absent_version_with_clean_summary`,
  `crates_io_probe_unreachable_index_fails_closed`,
  `crates_io_versions_for_tag_{maps_tag_families_to_crates,lockstep_maps_every_sharing_crate,excludes_custom_registry_crates}`,
  `run_force_bypasses_crates_io_probe`; end-to-end:
  `release_failure_default_rollback_reverts_bump_and_deletes_tag` now runs
  against a local 404 index responder and asserts exactly one probe hit
  during the auto-rollback path. cli suite fully green.

- [x] **npm publisher's transient-retry budget could exceed the `publish-npm`
  job `timeout-minutes`, guillotining the loop mid-publish (partial release) —
  RESOLVED 2026-07-06 (`b0eea2f1` + `4d7e2068`).** v0.15.0 run 28766146134
  stormed on one package (`npm publish attempt 1/10 … 7/10 failed (transient)`)
  and the 20m job timeout SIGKILLed it mid-attempt-7, leaving 4 platform
  packages live and the rest + metapackage absent. **Fixed:** `retry_sync`
  gained an optional wall-clock deadline (`retry_sync_deadline`); the npm
  publish sequence derives it once from a new optional `retry.max_elapsed`
  config (`Context::retry_deadline`) and stops before a backoff would blow the
  budget, exiting with the last error (recoverable via the idempotent
  `version_already_published` skip) and a distinct "retry budget exhausted;
  idempotent re-run resumes" breadcrumb instead of being guillotined.
  `.anodizer.yaml` sets `retry.max_elapsed: 15m` (< the 20m job timeout);
  default unset = GoReleaser attempt-count parity. **Evidence:** `retry.rs`
  deadline tests (already-elapsed → 1 attempt no-sleep; `None` == `retry_sync`;
  far-future no-op), `Context::retry_deadline` config → Some/None tests, npm
  retry tests — core 75/75, stage-publish 2506/2506. **0.15.0 recovery:**
  re-ran publish-npm run 28766146134 → success; full package set live.

- [x] **npm Trusted Publishing (OIDC provenance) fell back to `NPM_TOKEN` for
  every package (no provenance) — RESOLVED 2026-07-06 (anodizer-action
  `5629a41`).** NOT a server-side gap (the Trusted Publisher was configured):
  the anodizer-action dependency detector (`resolve_requirement` in
  `scripts/install/auto-detect-deps.sh`) satisfied the npm requirement the
  moment `npm` was on PATH with no version check. GitHub runners ship npm
  10.9.x (< the 11.5.1 OIDC floor), so the ambient stale npm suppressed the
  node/npm-floor install and every OIDC exchange fell back to the token.
  **Fixed** in anodizer-action `5629a41` (version-gate the on-PATH check
  against `NPM_DEFAULT_VERSION`; an under-floor binary falls through to install
  the upgrade); pushed, `v1` advanced. **Proven:** the v0.15.0 publish-npm
  re-run on the fixed `@v1` published with real provenance — 0 `falling back to
  NPM_TOKEN` warnings, Rekor attestation uploaded
  (`tj-smith47/anodizer/attestations/34060634`), unscoped `anodizer`
  metapackage + all 8 `@tj-smith47/anodizer-*` live at 0.15.0. (The 4 platform
  packages that published token-only on the first 0.15.0 run stay
  provenance-less and immutable; v0.15.1 publishes all fresh with provenance.)

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
