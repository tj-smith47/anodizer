# Backlog

Post-v0.4.0 work. None of these block the current release.

---

## T3.1 — God-functions over 150 lines (19 production fns)

| File | Fn | Lines |
|---|---|---|
| `crates/stage-build/src/run.rs` | `run` | 1,248 |
| `crates/stage-archive/src/run.rs` | `run` | 1,023 |
| `crates/stage-release/src/run.rs` | `run` | 1,012 |
| `crates/stage-announce/src/run.rs` | `announce_body` | 933 |
| `crates/stage-nfpm/src/run.rs` | `run` | 797 |
| `crates/cli/src/commands/release/mod.rs` | `run` | 675 |
| `crates/stage-changelog/src/run.rs` | `run` | 574 |
| `crates/stage-snapcraft/src/build_stage.rs` | `run` | 570 |
| `crates/stage-publish/src/winget.rs` | `publish_to_winget` | 553 |
| `crates/stage-docker/src/run.rs` | `run` | 546 |
| `crates/stage-flatpak/src/lib.rs` | `run` | 533 |
| `crates/cli/src/commands/check/config.rs` | `run_checks` | 503 |
| `crates/stage-publish/src/nix/publish.rs` | `publish_to_nix` | 497 |
| `crates/stage-publish/src/chocolatey/publish.rs` | `publish_to_chocolatey` | 488 |
| `crates/stage-makeself/src/lib.rs` | `run` | 480 |
| `crates/stage-msi/src/lib.rs` | `run` | 461 |
| `crates/stage-checksum/src/run.rs` | `run` | 460 |
| `crates/stage-publish/src/homebrew/publish_formula.rs` | `publish_to_homebrew` | 441 |
| `crates/stage-publish/src/aur.rs` | `publish_to_aur` | 417 |

`stage-build/src/run.rs`, `stage-release/src/github/mod.rs`, and
`stage-nfpm/src/run.rs` each hit **14 levels of nesting**; `stage-docker`
and `stage-archive` hit 13.

**`announce_body` (933 lines)** is the highest-leverage single extraction:
chain of `if let Some(cfg) = announce.X` for ~15 providers, each block
structurally identical. A `trait Announcer { fn send(...); }` with one
`impl` per provider collapses this to a 20-line dispatch loop.

---

## W1 — Rollback warn-tests: live logger capture

Replace the helper-string assertion shim in
`<publisher>_rollback_warns_when_no_targets_recorded` tests with a real
logger-capture sink (`tracing-test` or custom `tracing::Subscriber` shim)
that asserts on the emitted log line. Retire or repurpose
`rollback_empty_warning_msg`.

**Reference:** search `rollback_empty_warning_msg` across `crates/stage-publish/src/`.

---

## W2 — Typed `PublishEvidenceExtra` enum

Replace `PublishEvidence.extra: serde_json::Value` with a typed enum
(`PublishEvidenceExtra::Homebrew(...) | ::Krew(...) | ...`) so the type
system prevents credential leakage structurally, instead of relying on
per-publisher `#[serde(skip)]` discipline + negative tests.

**Reference:** `crates/core/src/publish_evidence.rs` + every
`_target_extra_carries_no_secret_material` test.

---

## D1 — Cargo `.crate` packaging determinism

Byte-stable `cargo package` output for crates.io re-verification. `cargo
package` leaks non-determinism via file mtimes, `.cargo_vcs_info.json`,
and tar ordering. Currently accepted via slot-skip (skip when the version
already exists on crates.io).

Harness extension: `--stages=cargo-package` fixture mode.

**Reference:** `crates/stage-publish/src/cargo.rs`.
**Research:** cargo issue `#10718`, `repro.rs`, `reproducible-builds.org`.

---

## D2 — Docker BuildKit reproducible builds

Byte-stable Docker image digest (and manifest list for multi-arch) across
rebuilds of the same commit. BuildKit `--rewrite-timestamp` + cosign
attestation timestamp interplay is non-obvious.

Harness extension: `--stages=docker` fixture mode.

**Reference:** `crates/stage-docker/`.

---

## D3 — Installer-tool determinism sweep

Byte-stable installers across six tools: rpmbuild, wix/candle/light,
makensis, hdiutil, pkgbuild/productbuild, dpkg-deb/nfpm. Each has its
own reproducibility story and signature-interaction concerns.

Harness extension: `--stages=installers` fixture mode covering all six.

**Reference:** `crates/stage-srpm/`, `crates/stage-msi/`,
`crates/stage-nsis/`, `crates/stage-dmg/`, `crates/stage-pkg/`,
`crates/stage-nfpm/`.

---

## V1 — Winget v0.4.0 empty `InstallerSha256` root cause

The v0.4.0 winget manifest published with `InstallerSha256: ''` for both
arm64 and x64, tripping the winget validation pipeline
(PR microsoft/winget-pkgs#379056, label `Manifest-Validation-Error`).

The immediate fix landed in `crates/stage-publish/src/winget.rs`
(grep `winget: archive '` and `winget: portable binary '`) — winget
now bails with an actionable error when an archive or portable binary
artifact arrives without `sha256` metadata, so the broken manifest
cannot ship again. The bail is a precondition, not a circumvention:
the stage now refuses to construct a manifest that winget validation
would reject.

The deeper root cause is **why** the artifact `sha256` metadata was
empty when winget ran. Two candidates:

1. The v0.4.0 Release run executed a publish-only flow over assets
   downloaded fresh from the GitHub release, and the publish-only path
   does not re-seed `sha256` metadata for downloaded archive artifacts
   (only `refresh_combined_checksums` runs, which rewrites the
   combined `checksums.txt` but does not appear to update individual
   artifact metadata).
2. The checksum stage was skipped or ran after winget for that flow.

Investigation steps:
- Inspect the v0.4.0 Release workflow log to see the stage order and
  whether stage-checksum ran before stage-publish/winget.
- Audit `refresh_combined_checksums` in `stage-checksum/src/run.rs`:
  does it write `sha256` back to each artifact's metadata in addition
  to rewriting the combined sums file?
- If publish-only is the offender, extend the seed step to populate
  per-artifact `sha256` metadata from the downloaded asset bytes.

This work blocks the next winget submission for any release that
re-runs publish-only against existing GitHub assets.

---

## T2 — Virtual-time-friendly rate_limit sleep

`crates/stage-release/src/github/rate_limit.rs::check_github_rate_limit`'s
sleep-until-reset and the past-reset 5 s floor cannot be unit-tested
under `tokio::test(start_paused = true)` because the production
function performs real HTTPS I/O via reqwest. Tokio's auto-advance
only fires when the runtime is fully idle (no pending I/O), so the
runtime waits indefinitely for the responder while the virtual timer
never ticks.

**Fix candidates:**

1. Inject the sleep helper: change `check_github_rate_limit` to take
   `&dyn AsyncSleep` (or a `Sleep` callback) so tests pass a controlled
   sleeper that records duration without actually sleeping.
2. Add an in-runtime tokio-driven responder helper (alongside the
   std::thread `spawn_oneshot_https_responder`) so all socket I/O runs
   on the same runtime and `start_paused` auto-advance interleaves
   correctly.

Either unblocks pinning the two uncovered branches. Track here so the
next coverage pass doesn't re-add the flaky virtual-time tests.

---

## T1 — cwd+PATH-injectable git/gh APIs for testability

Two functions in `crates/core/src/git/github_api.rs` cannot be unit-tested
without process-wide env mutation that races every other parallel test:

1. `create_tag_via_github_api` reads `std::env::current_dir()`
   internally for the remote-detection step. The "errs outside a git
   repo" branch is uncoverable without `set_current_dir`, which races
   every other test that subprocess-spawns `git`.
2. `gh_api_get` / `gh_api_get_paginated` / the third `gh`-spawning fn
   in this file all do `Command::new("gh")` and inherit `PATH`. The
   "gh missing from PATH" branch is uncoverable without mutating
   global `PATH`, which races every other test that spawns `git`,
   `rustc`, etc. (Both failure modes were observed once
   in this session — 3 git tests and 8 core tests went red intermittently
   until the PATH/CWD-mutating tests were removed.)

**Fix**: add injectable overloads:

- `create_tag_via_github_api_in(cwd: &Path, ...)` (current fn becomes
  a thin wrapper passing `std::env::current_dir()?`).
- `gh_api_get_with_binary(gh_binary: &Path, endpoint, token)` plus
  paginated sibling. Existing wrappers default to `Path::new("gh")`.

Tests then point at a real tempdir cwd / a non-existent binary path
without ever touching process-wide state. Apply the same pattern to any
other `core::git::*` function that reads `current_dir()` or hardcodes a
binary name — grep for `current_dir()` and `Command::new(` in
`crates/core/src/git/`.

---

## V2 — Silent-default-empty sweep (BLOCKER for next release)

The v0.4.0 winget bug was a `unwrap_or_default()` on missing required
metadata that serialised to `''` and shipped to a downstream registry
that then rejected the manifest. The pattern is **silent default for a
value the downstream contract requires to be non-empty.** That pattern
likely repeats across other publishers and stages — each one is a
trap waiting for the next release to spring it.

**Scope of audit (do this before pushing the next release):**

1. **Grep every `unwrap_or_default()`, `unwrap_or_else(.. String::new)`,
   `unwrap_or("")` site in `crates/stage-publish/**` and `crates/stage-*/**`**
   that flows into a downstream payload (manifest, formula, request body,
   git commit, file write). For each, ask:
   - "If the source metadata is genuinely missing, would the downstream
     accept the empty value or reject it?"
   - If reject — replace with `bail!` carrying an actionable message
     (operator + remediation pointer), mirroring the winget fix.
   - If accept — leave alone, add a one-line WHY comment explaining
     the silent default is correct.

2. **Per-publisher checklist of required-non-empty fields** the
   downstream registry validates. Sources of truth:
   - winget: schema at `aka.ms/winget-manifest.installer.1.12.0.schema.json`
   - homebrew: `brew audit --strict` rules
   - chocolatey: nuspec validation (`InstallerSha256` equivalent: `checksum`)
   - scoop: bucket validation
   - krew: krew plugin schema
   - nix: nixpkgs review checklist
   - aur: PKGBUILD lint
   - cargo: cargo publish API rejects empty `description`, `license`
   - snapcraft: `snapcraft.yaml` validation
   - winget product_code, locale fields, sha256 (already fixed)

3. **Other surfaces beyond publishers** to sweep:
   - `stage-release/src/github` — `body`, `name`, `tag_name`, `target_commitish`
     emitted with empty defaults?
   - `stage-announce/*` — empty webhook payloads silently posted?
   - `stage-changelog/*` — empty changelog body?
   - `stage-source/*` — empty archive name?
   - `stage-archive/*` — empty checksums?

4. **Output**: a fix-list (or note "verified non-trap" per site). Bundle
   the fixes into one commit with regression tests per bail, matching
   the winget pattern (`*_without_X_metadata_bails_with_actionable_error`).

**Why this is a release-blocker**: every silent default is a future
support ticket from a user whose release shipped a broken artifact.
Each one is also a winget-style multi-week feedback loop (publisher
accepts → external service validates async → operator notified later).
Fail-loud at the source is the cheapest fix point.

---

## C1 — Coverage gaps left by the 2026-05-24 fixture session

The "test fixtures" session built env_mutex, build_test_octocrab,
classify_pr_transport, and the ANODIZER_GITHUB_API_BASE override, then
used them to bring 8 low-coverage files up to the 60-85% range. These
gaps were identified during the same pass but were either out of scope
for fixture work or required production refactors that the session
intentionally avoided.

### `crates/stage-release/src/github/mod.rs`

- `run_github_backend` (~900 LOC orchestrator): testing requires a full
  Context + tokio::Runtime + multi-response responder fixture plus a
  fake filesystem for `std::fs::read(&path)`. Worth its own focused
  session; piecemeal stubbing risks tautology.
- Inner `find_draft_by_name` async closure: defined inside
  `run_github_backend`, not extractable without a production-code edit.
  Promote to a `pub(crate)` free fn first, then test.
- Upload-loop bespoke 422 / secondary-rate-limit / primary-rate-limit
  retry chain (lines 720-988): tightly coupled to
  `octocrab::Octocrab::repos().releases().upload_asset()` which has no
  public response-injection surface. Needs either a refactor extracting
  an `UploadAttemptOutcome` classifier (pure) or a full HTTP-fixture
  harness with a TLS-capable responder.

### `crates/stage-release/src/github/rate_limit.rs`

Four branches uncovered because they hardcode `https://api.github.com/rate_limit`:
non-2xx status (line 46), malformed JSON (line 51), `remaining > threshold`
early-return (line 64), sleep-until-reset (lines 68-129). The current
raw-TCP responder can't intercept HTTPS paths. Options:
- Add a rustls-backed test responder (self-signed cert + reqwest's
  `danger_accept_invalid_certs`), OR
- Extend the `ANODIZER_GITHUB_API_BASE` pattern to this module so the
  responder works over plain HTTP, OR
- Extract the rate-limit decision logic to a pure classifier and unit
  test that.

### Other low-coverage files (not touched this session)

These weren't part of fixture scope but remain low-coverage:
- `crates/stage-srpm/src/lib.rs` (~58% line)
- Various stage `run.rs` files in the 30-60% range

Each likely has its own production-shape constraints. Audit individually
before assuming the fixture toolkit applies.
