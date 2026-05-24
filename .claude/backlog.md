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
