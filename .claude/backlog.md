# Backlog

**Execution contract for the next session:**

- Use **subagent-driven development** on master directly (no worktrees).
- Each task gets a **code review subagent** after the implementing
  subagent returns — review the actual diff, not the report.
- Complete **every task in this file, IN FULL, nothing skipped.**
- **Do not add new entries to this file** during execution. Discoveries
  that would have created a follow-up task get fixed in-flight instead.
- **Do not add anything to `.claude/known-bugs.md`** during execution.
- **No tautological tests.** Every test pins observable behavior that
  would catch a real regression. `assert_eq!(CONST, CONST)`,
  `assert!(true)`, structure-without-behavior round-trips — banned.
- **No session-narrative comments** in source. The
  `~/.claude/rules/critical.md` rule #9 list (Plan/Phase/Task/Step/…)
  applies. Reviewers reject violators.
- Bundle small + independent items into one implementer; split when
  items are large or coupled. Spawn the full team roster at dispatch
  time so review subagents are ready when implementers return.

---

## V1 — Winget v0.4.0 empty `InstallerSha256` upstream root cause

The publish-only pipeline now backfills sha256 via the head
`ChecksumStage` (see `crates/cli/src/pipeline.rs::build_publish_only_pipeline`),
which closes the bug for future ship cycles. The remaining question is
**why** the GHA shard-merge dropped per-artifact `sha256` metadata in
the first place: each determinism shard uploads its own `dist/artifacts.json`,
the release job `download-artifact`s them all with `merge-multiple: true`,
and the merged `dist/artifacts.json` ends up missing sha256 entries for
artifacts that came from earlier shards.

**Investigate**:
- Walk `crates/cli/src/commands/release/publish_only.rs` and trace where
  `dist/artifacts.json` is loaded vs. where the determinism harness wrote
  it. If `download-artifact merge-multiple` overwrites instead of merges,
  the right fix is a `merge_artifacts_json` step in publish-only that
  unions per-shard catalogs before loading. (`actions/download-artifact@v4`
  with multiple `dist-<shard>` patterns will overwrite same-named files.)
- Add an integration test that reconstructs the shard-merge layout (two
  partial `artifacts.json` files merged) and asserts the union has both
  shards' sha256 entries.

The current `ChecksumStage`-at-head fix re-hashes from on-disk bytes,
which is correct but redundant if the catalog had survived the merge.

---

## V2 — Silent-default-empty sweep across publishers + stages

The v0.4.0 winget bug was a `unwrap_or_default()` on missing required
metadata that serialised to `''` and shipped to a registry that
rejected the manifest. The pattern is **silent default for a value the
downstream contract requires to be non-empty.** Every other such site
in the publish surface is a trap of the same shape.

**Scope:**

1. Grep every `unwrap_or_default()`, `unwrap_or_else(.. String::new)`,
   `unwrap_or("")` site in `crates/stage-publish/**` and `crates/stage-*/**`
   that flows into a downstream payload (manifest, formula, request
   body, git commit, file write).
2. For each site decide:
   - **Reject** — replace with `bail!` carrying an actionable message
     mirroring the winget pattern. Add a regression test named
     `<publisher>_<field>_empty_metadata_bails_with_actionable_error`.
   - **Accept** — leave silent default in place, add a one-line WHY
     comment explaining the downstream tolerates empty.
3. Cross-reference per-publisher schemas: winget, homebrew formula +
   cask, chocolatey nuspec, scoop bucket, krew plugin, nix nixpkgs,
   AUR PKGBUILD, cargo publish, snapcraft.yaml.
4. Sweep beyond publishers: stage-release github body/name/tag/commitish,
   stage-announce webhook payloads, stage-changelog body, stage-source
   archive name, stage-archive checksum file names.

Output: one commit per publisher group + the cross-publisher sweep
crate. Bundle the regression tests with the bails.

---

## T1 — cwd+PATH-injectable git/gh APIs

Two functions in `crates/core/src/git/github_api.rs` cannot be unit-tested
without mutating process-wide state that races every other parallel test:

1. `create_tag_via_github_api` reads `std::env::current_dir()` for the
   remote-detection step.
2. `gh_api_get` / `gh_api_get_paginated` / the third `gh`-spawning fn
   in this file all do `Command::new("gh")` and inherit `PATH`.

**Fix**: add injectable overloads.

- `create_tag_via_github_api_in(cwd: &Path, ...)` (existing fn becomes
  a thin wrapper passing `std::env::current_dir()?`).
- `gh_api_get_with_binary(gh_binary: &Path, endpoint, token)` plus
  paginated sibling.

Then add tests covering the missing-repo and missing-gh-binary
branches against a real tempdir / nonexistent path — no process-wide
mutation, no `#[serial]` decoration needed.

Apply the same pattern to every other `core::git::*` function that
reads `current_dir()` or hardcodes a binary name — grep both in
`crates/core/src/git/`.

---

## T2 — Injectable sleep / runtime-driven HTTPS responder for rate_limit

`crates/stage-release/src/github/rate_limit.rs::check_github_rate_limit`'s
sleep-until-reset and the past-reset 5 s floor cannot be unit-tested
under `tokio::test(start_paused = true)`: tokio's auto-advance only
fires when the runtime is fully idle, and the std::thread responder's
real socket I/O keeps the runtime non-idle. The two virtual-time tests
hang indefinitely.

**Fix**: pick one of the two.

A. Inject the sleep helper: change `check_github_rate_limit` to take
   `&dyn AsyncSleep` (or a small `Sleep` callback) so tests pass a
   controlled sleeper that records duration without sleeping.
B. Add a tokio-driven HTTPS responder (sibling of the std::thread
   `spawn_oneshot_https_responder`) so socket I/O runs on the same
   runtime and `start_paused` auto-advance interleaves correctly.

Either unblocks tests for both branches. Pick whichever is cheaper
after reading the production fn — A is one trait + 5 lines; B reuses
`hyper` + `rustls` machinery already in the deps.

---

## T3 — Reduce `unsafe` env mutation in tests

191 `unsafe { std::env::set_var(...) }` / `remove_var(...)` sites across
the test surface. Root cause: 30+ production sites read env vars
directly (`GITHUB_TOKEN`, `ANODIZER_GITHUB_API_BASE`, `DOCKER_USERNAME`,
`MCP_GITHUB_TOKEN`, `CI_JOB_TOKEN`, etc.) so tests must mutate the
process env to exercise branches. Rust 2024 marked `set_var` /
`remove_var` `unsafe` because they race with libc env reads.

**Fix**: every production env read becomes a Context lookup.

1. Add `Context::env_var(name: &str) -> Option<String>` that delegates
   to a `Box<dyn EnvSource>` field. Default impl reads `std::env::var`;
   tests inject a `HashMap`-backed source.
2. Replace each production `std::env::var(...)` with `ctx.env_var(...)`.
   This touches: gitlab.rs (CI_JOB_TOKEN, CI_SERVER_VERSION),
   secondary_rate_limit.rs (ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS),
   artifactory.rs (ARTIFACTORY_TOKEN/SECRET), branch.rs + rate_limit.rs
   (ANODIZER_GITHUB_API_BASE), snapcraft/build_stage.rs (HOME),
   scope.rs (multiple), http_upload.rs, mcp/auth.rs (3 vars),
   dockerhub.rs (DOCKER_USERNAME + secret_env_var), homebrew/publisher.rs
   (GITHUB_TOKEN family), scoop.rs (same), lib.rs.
3. Convert tests in the same crates from `unsafe set_var` to
   `TestContextBuilder.env("KEY", "value")`.

Acceptance: zero `unsafe { std::env::set_var }` / `remove_var` in
`crates/**/src/**/tests/` and in the inline `#[cfg(test)]` modules,
except for the documented `env_mutex`-protected RAII pattern in
`test_helpers/env.rs` (which itself is the only legitimate site).

---

## Coverage push: 82.6% → ≥90% line

Top remaining low-coverage files by absolute uncovered-line count
(from cobertura.xml after the C-wave):

| Cov% | Uncov | File |
|---|---|---|
| 73.1% | 500 | `crates/stage-notarize/src/lib.rs` |
| 70.8% | 488 | `crates/stage-publish/src/krew.rs` |
| 67.2% | 458 | `crates/core/src/template/base_tera.rs` |
| 68.1% | 430 | `crates/stage-publish/src/artifactory.rs` |
| 57.0% | 401 | `crates/stage-build/src/run.rs` |
| 56.5% | 367 | `crates/stage-release/src/run.rs` |
| 79.0% | 365 | `crates/stage-publish/src/winget.rs` |
| 24.5% | 359 | `crates/stage-publish/src/nix/publish.rs` |
| 72.7% | 348 | `crates/stage-publish/src/cloudsmith.rs` |
| 61.3% | 334 | `crates/stage-docker/src/run.rs` |
| 80.0% | 330 | `crates/stage-publish/src/scoop.rs` |
| 52.9% | 322 | `crates/stage-publish/src/homebrew/cask.rs` |
| 63.7% | 308 | `crates/cli/src/commands/release/split.rs` |
| 78.8% | 300 | `crates/stage-publish/src/cargo.rs` |
| 68.7% | 273 | `crates/stage-publish/src/dockerhub.rs` |
| 38.4% | 273 | `crates/stage-publish/src/chocolatey/publish.rs` |
| 58.6% | 270 | `crates/stage-release/src/gitlab.rs` |
| 66.1% | 269 | `crates/stage-sbom/src/lib.rs` |
| 63.0% | 265 | `crates/stage-publish/src/util/pr.rs` |
| 42.5% | 255 | `crates/stage-publish/src/homebrew/publish_formula.rs` |
| 22.7% | 254 | `crates/stage-publish/src/homebrew/publish_top.rs` |

**Approach for the next session:**

- Dispatch one implementer subagent per crate (bundle small siblings).
- Each subagent uses the existing harnesses:
  `anodizer_core::test_helpers::{TestContextBuilder, env::env_mutex,
  artifact_set::TestArtifactSet, responder, scripted_responder,
  https_responder}` and the stage-release `test_support::build_test_octocrab`.
- After every implementer returns, dispatch a code reviewer for the
  same crate. Reviewer reads the diff and rejects any tautology,
  session-narrative comment, or test that fails the "what regression
  does this catch?" question.
- Run `task coverage:check` after each batch settles (NOT between
  subagents while they're still writing — shared `target/` contention).

Target: workspace ≥ 90% line.

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

**`announce_body` (933 lines)** is the highest-leverage single
extraction: chain of `if let Some(cfg) = announce.X` for ~15 providers,
each block structurally identical. A `trait Announcer { fn send(...); }`
with one `impl` per provider collapses this to a 20-line dispatch loop.

Decompose each, then push coverage on the smaller pieces.

---

## W1 — Rollback warn-tests: live logger capture

Replace the helper-string assertion shim in
`<publisher>_rollback_warns_when_no_targets_recorded` tests with a real
logger-capture sink (`tracing-test` or custom `tracing::Subscriber` shim)
that asserts on the emitted log line. Retire or repurpose
`rollback_empty_warning_msg`.

Reference: grep `rollback_empty_warning_msg` across `crates/stage-publish/src/`.

---

## W2 — Typed `PublishEvidenceExtra` enum

Replace `PublishEvidence.extra: serde_json::Value` with a typed enum
(`PublishEvidenceExtra::Homebrew(...) | ::Krew(...) | ...`) so the type
system prevents credential leakage structurally instead of relying on
per-publisher `#[serde(skip)]` discipline + negative tests.

Reference: `crates/core/src/publish_evidence.rs` + every
`_target_extra_carries_no_secret_material` test.

---

## D1 — Cargo `.crate` packaging determinism

Byte-stable `cargo package` output for crates.io re-verification.
`cargo package` leaks non-determinism via file mtimes,
`.cargo_vcs_info.json`, and tar ordering. Currently accepted via
slot-skip (skip when the version already exists on crates.io).

Harness extension: `--stages=cargo-package` fixture mode.

Reference: `crates/stage-publish/src/cargo.rs`.
Research: cargo issue `#10718`, `repro.rs`, `reproducible-builds.org`.

---

## D2 — Docker BuildKit reproducible builds

Byte-stable Docker image digest (and manifest list for multi-arch)
across rebuilds of the same commit. BuildKit `--rewrite-timestamp` +
cosign attestation timestamp interplay is non-obvious.

Harness extension: `--stages=docker` fixture mode.

Reference: `crates/stage-docker/`.

---

## D3 — Installer-tool determinism sweep

Byte-stable installers across six tools: rpmbuild, wix/candle/light,
makensis, hdiutil, pkgbuild/productbuild, dpkg-deb/nfpm. Each has its
own reproducibility story and signature-interaction concerns.

Harness extension: `--stages=installers` fixture mode covering all six.

Reference: `crates/stage-srpm/`, `crates/stage-msi/`,
`crates/stage-nsis/`, `crates/stage-dmg/`, `crates/stage-pkg/`,
`crates/stage-nfpm/`.
