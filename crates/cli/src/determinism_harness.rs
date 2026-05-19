//! Determinism harness — drives N from-clean rebuilds in hermetic
//! worktrees and diffs the emitted artifacts.
//!
//! Spec: `.claude/specs/2026-05-14-release-resilience.md#verification-harness-cli`.
//!
//! ## Shape
//!
//! ```ignore
//! let harness = Harness {
//!     repo_root, commit, stages, runs, sde, allowlist, report_path,
//! };
//! let report: DeterminismReport = harness.run()?;
//! ```
//!
//! The harness:
//!
//! 1. For each of `runs` runs, opens a fresh
//!    [`anodizer_core::git::worktree::Worktree`] rooted at `commit`.
//! 2. Builds an isolated env: per-run `CARGO_HOME`, `CARGO_TARGET_DIR`,
//!    `TMPDIR`, `HOME`; `SOURCE_DATE_EPOCH=self.sde`; `PATH` inherited
//!    from the host via [`allow_listed_path()`] (two harness runs from
//!    the same host process see identical PATH, so determinism holds);
//!    plus an identity-only allow-list — see [`HARNESS_ENV_ALLOWLIST`].
//!    The remaining shape is platform-specific:
//!
//!    - **Linux / macOS**: everything else is stripped. Unix env is
//!      sparse and well-understood; an explicit identity-only allow-list
//!      is the safest contract.
//!    - **Windows**: inherits the FULL host env, then drops everything
//!      covered by [`windows_env_should_drop`]: a credential-bearing
//!      deny-list (`GITHUB_TOKEN`, `CARGO_REGISTRY_TOKEN`, `AWS_*`,
//!      cosign / gpg / docker / chocolatey / snapcraft creds,
//!      anodize-publisher creds), the GH Actions workflow internals
//!      (`ACTIONS_*`, `RUNNER_TOKEN`), a defense-in-depth credential
//!      suffix sweep (`_TOKEN`, `_KEY`, `_SECRET`, `_PASSWORD`,
//!      `_PASSPHRASE`, `_CREDENTIALS`), AND a hermeticity sweep that
//!      drops every `GITHUB_*` / `RUNNER_*` not on the identity-only
//!      [`HARNESS_ENV_ALLOWLIST`] (e.g. `RUNNER_TEMP`,
//!      `RUNNER_TOOL_CACHE`, `RUNNER_WORKSPACE`, `GITHUB_WORKSPACE`,
//!      `GITHUB_EVENT_PATH`). Rationale: Windows env is sprawling —
//!      cc-rs / cargo / rustc need `PROCESSOR_ARCHITECTURE`,
//!      `PROGRAMFILES*`, `WINDIR`, `SystemRoot`, `USERPROFILE`,
//!      `APPDATA`, `LOCALAPPDATA`, `TEMP`, `TMP`, `PATHEXT`, plus the
//!      entire MSVC toolchain block (`VC*` / `VS*` / `INCLUDE` / `LIB`
//!      / `LIBPATH` / `WindowsSdk*` / `UCRT*`) and likely more.
//!      Enumerating each in an allow-list is whack-a-mole; an inverse
//!      skip predicate is the auditable contract. Audit reference
//!      `.claude/audits/2026-05-15-release-resilience-review.md#i7`
//!      asked for "no credentials leak through" AND "no host workflow
//!      state leaks through" (the original design excluded
//!      `RUNNER_TEMP` for this reason); the skip predicate upholds both
//!      contracts on Windows.
//! 3. Invokes the build-side pipeline (`anodize release --snapshot
//!    --skip=<SIDE_EFFECT_STAGES>`, see
//!    [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`]) inside
//!    the worktree with that env.
//! 4. Walks `<worktree>/dist`, SHA256s every file, returns a
//!    `BTreeMap<artifact_name, hash>` for the run.
//! 5. Once all runs complete, diffs the maps and constructs a
//!    [`anodizer_core::DeterminismReport`]. Allow-listed artifacts (the
//!    compile-time + runtime lists carried on `self.allowlist`) are
//!    excluded from `drift_count` but still appear in `artifacts` and
//!    (with per-run hashes) in `drift`.
//!
//! ## Implementation choice: shell to `current_exe`
//!
//! The harness shells out to the currently-running `anodizer` binary
//! rather than calling [`crate::pipeline::build_release_pipeline`]
//! directly. Rationale: a) `Context` setup in-process requires re-parsing
//! the config + re-deriving the SDE + reconciling all the global flags,
//! reproducing logic that already lives in `main.rs`; b) shelling out
//! gives true env isolation (we can `env_clear` on the child without
//! touching the harness process); c) the binary on disk is what the
//! release pipeline ships, so byte-stability of *that* binary is what we
//! actually want to assert. The spec's "Pragmatic deferrals" callout
//! explicitly green-lights this path.
//!
//! ## Allow-list semantics
//!
//! Allow-list matching uses the same `*.ext` glob semantics as
//! [`anodizer_core::DeterminismState::resolve_reason`]: a leading `*` is
//! a suffix-match, anything else is exact-match. Compile-time matches
//! win on collision; the matched reason populates
//! [`ArtifactRow::nondeterministic_reason`].

use anodizer_core::git::worktree::Worktree;
use anodizer_core::harness_signing::EphemeralSigningKeys;
use anodizer_core::{AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Stage subset selector for `--stages=<subset>`.
///
/// Currently informational: every variant maps to "run the build-side
/// pipeline and look at the artifacts that stage produces". The harness
/// shells to `anodize release --snapshot --skip=...` which runs the full
/// build-side pipeline; finer-grained per-stage gating is a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageId {
    Build,
    Source,
    Upx,
    Archive,
    Nfpm,
    Sbom,
    Sign,
    Checksum,
}

impl StageId {
    /// Lowercase canonical name, matching `--stages=` CLI tokens and the
    /// `stages_under_test` array in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            StageId::Build => "build",
            StageId::Source => "source",
            StageId::Upx => "upx",
            StageId::Archive => "archive",
            StageId::Nfpm => "nfpm",
            StageId::Sbom => "sbom",
            StageId::Sign => "sign",
            StageId::Checksum => "checksum",
        }
    }
}

/// Preamble stages the child release subprocess MUST keep enabled
/// regardless of `--stages=`. These don't produce per-target artifacts
/// the harness diffs, but the pipeline needs them to function:
///
/// - `validate` — config / target / signing-cred validation.
/// - `before` — user `before:` hooks (e.g. codegen).
/// - `changelog` — populates release notes context downstream stages may
///   read; cheap and side-effect-free in `--snapshot` mode.
/// - `templatefiles` — pre-build template materialization.
///
/// Adding any of these to the child `--skip=` list would break stages
/// that depend on their side-effects-on-context (not on disk), which is
/// why the harness's complement-set calculation subtracts them.
const PRESERVE_SET: &[&str] = &["validate", "before", "changelog", "templatefiles"];

/// Compute the harness's child-subprocess "extra skip" set — every stage
/// name in [`anodizer_core::context::VALID_RELEASE_SKIPS`] that is NOT:
///
/// - in the operator's requested-stages list (`requested`), OR
/// - in [`PRESERVE_SET`] (preamble helpers the pipeline needs), OR
/// - already in
///   [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`] (the
///   runner merges those in unconditionally; subtracting them here just
///   keeps the returned list lean).
///
/// The result is fed into
/// [`anodizer_core::determinism_runner::run_build_pipeline_subprocess`]
/// as the `extra_skip` argument; the runner merges + dedupes against
/// `SIDE_EFFECT_STAGES` before joining into `--skip=<csv>`.
///
/// Why this matters: `--stages=` is the harness's "what to diff" filter,
/// but it does NOT restrict which stages the child release subprocess
/// runs. Without this complement set the child runs the full pipeline
/// (minus side-effects), including produce-stages like `nfpm`, `nsis`,
/// `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`, `appbundle`,
/// `srpm`, `upx`, `makeself`, `notarize`. On macOS / Windows shards
/// those binaries aren't installed; on Linux shards some are but the
/// target artifacts don't exist on a non-native shard. Skipping every
/// non-requested produce-stage matches the spec's "only exercise the
/// requested stages" contract.
fn compute_extra_skip(requested: &[StageId]) -> Vec<String> {
    use anodizer_core::context::VALID_RELEASE_SKIPS;
    use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
    let requested_names: BTreeSet<&str> = requested.iter().map(|s| s.as_str()).collect();
    VALID_RELEASE_SKIPS
        .iter()
        .copied()
        .filter(|name| !requested_names.contains(name))
        .filter(|name| !PRESERVE_SET.contains(name))
        .filter(|name| !SIDE_EFFECT_STAGES.contains(name))
        .map(str::to_string)
        .collect()
}

/// Harness configuration. Constructed by the CLI dispatcher
/// (`crate::commands::check::determinism::run`) and consumed once via
/// [`Harness::run`].
pub struct Harness {
    /// Repository root that owns the worktrees the harness will spawn.
    pub repo_root: PathBuf,
    /// Full commit SHA the harness rebuilds. Each run does
    /// `git worktree add --detach <tmp> <commit>`.
    pub commit: String,
    /// Stage subset under test. Surfaced into the report's
    /// `stages_under_test` field.
    pub stages: Vec<StageId>,
    /// Number of from-clean rebuilds to perform.
    pub runs: u32,
    /// `SOURCE_DATE_EPOCH` value to export into every run's subprocess
    /// env. Resolved by the CLI dispatcher (snapshot resolver under
    /// `--snapshot`, HEAD commit timestamp otherwise).
    pub sde: i64,
    /// Compile-time + runtime allow-lists used to exclude artifacts from
    /// `drift_count` (entries still appear in `artifacts` and `drift`).
    pub allowlist: AllowList,
    /// Destination path for the JSON report. Currently informational —
    /// the CLI dispatcher owns writing the file. Surfaced here so future
    /// changes (e.g. mid-run streaming) have somewhere natural to land.
    #[allow(dead_code)]
    pub report_path: PathBuf,
    /// `--inject-drift=<stage>` (test-harness gated): after each run
    /// completes, append one random byte to the first artifact whose
    /// inferred stage equals this value. Forces the harness to detect
    /// drift across runs so integration tests can verify the report
    /// shape on the failure path. `None` outside the
    /// `ANODIZE_TEST_HARNESS=1` env (rejected upstream by the CLI
    /// dispatcher).
    pub inject_drift: Option<String>,
    /// `--targets=<csv>`: restrict the harness to a subset of configured
    /// target triples. Forwarded to the child `anodize release
    /// --snapshot` subprocess as `--targets=<csv>` so the rebuild only
    /// touches buildable targets on this runner. `None` validates every
    /// configured target (legacy single-runner behavior). The sharded
    /// `release.yml` job matrix supplies this so the Linux runner skips
    /// Apple/Windows targets (and vice versa) — cross-compile would
    /// otherwise fail at link time on missing SDKs.
    pub targets: Option<Vec<String>>,
}

impl Harness {
    /// Drive the harness end-to-end and return the populated report.
    ///
    /// Does NOT write the report — the CLI dispatcher is responsible for
    /// serializing the returned `DeterminismReport` and exiting non-zero
    /// when `drift_count > 0`. Keeping write logic out of the harness
    /// keeps unit tests pure (no temp-file I/O for the report itself).
    pub fn run(&self) -> Result<DeterminismReport> {
        let mut per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>> =
            Vec::with_capacity(self.runs as usize);

        // Provision once: both runs must sign with identical key
        // material, otherwise even byte-deterministic GPG signatures
        // would diverge.
        let signing_keys: Option<EphemeralSigningKeys> = if self.stages.contains(&StageId::Sign) {
            Some(anodizer_core::harness_signing::provision_ephemeral_keys(
                self.sde,
            )?)
        } else {
            None
        };

        for run_idx in 0..self.runs {
            // Default to <repo_root>/.det-worktrees/ — keeps the harness
            // off `/tmp` (which is tmpfs on many distros and exhausts
            // fast when the cargo target dir lives inside the worktree).
            // CI (GitHub Actions) sets RUNNER_TEMP to a disk-backed path
            // outside the repo, so honor that when present.
            let worktree_root = std::env::var_os("RUNNER_TEMP")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| self.repo_root.join(".det-worktrees"));
            // Ensure the parent exists; `git worktree add` won't create
            // intermediate directories.
            let _ = std::fs::create_dir_all(&worktree_root);
            // Worktree path MUST be identical across runs. Some
            // dependencies embed their absolute cargo-registry path as
            // UTF-16LE inside the compiled binary (commonly seen on
            // Windows where Win32 APIs need wchar paths). Rust's
            // `--remap-path-prefix` operates on UTF-8 source-path
            // references only and does NOT touch UTF-16-encoded
            // strings. If the worktree path differs between runs (e.g.
            // `...-run-0` vs `...-run-1`), those 2 bytes propagate
            // into the binary, /Brepro hashes the new content,
            // produces a different COFF TimeDateStamp, and cascades
            // into every artifact wrapping the binary.
            //
            // Using a constant path with `remove_dir_all` between
            // runs collapses both runs onto identical absolute paths
            // and eliminates the UTF-16 drift entirely.
            let worktree_path = worktree_root.join("anodize-determinism");
            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            let env = self.build_isolated_env(&worktree, signing_keys.as_ref())?;
            self.run_build_pipeline(worktree.path(), &env)
                .with_context(|| format!("building pipeline for determinism run {}", run_idx))?;
            let artifacts = discover_artifacts(worktree.path())?;
            // `--inject-drift=<stage>` (test-harness gated): mutate the
            // first artifact of the named stage before hashing so the
            // report records drift. The miss path logs the discovered
            // artifact set so a silent "found no matching stage" in CI
            // is debuggable from logs alone.
            if let Some(stage) = self.inject_drift.as_deref() {
                match pick_first_artifact_for_stage(&artifacts, stage) {
                    Some(victim) => {
                        inject_drift_byte(victim).with_context(|| {
                            format!(
                                "injecting drift byte into {} on run {}",
                                victim.display(),
                                run_idx
                            )
                        })?;
                    }
                    None => {
                        let summary: Vec<String> = artifacts
                            .iter()
                            .map(|p| {
                                let s = p.to_string_lossy();
                                format!("  {} -> {}", p.display(), infer_stage_from_path(&s))
                            })
                            .collect();
                        eprintln!(
                            "warn: --inject-drift={} matched no artifact on run {}; \
                             discovered artifacts ({}):\n{}",
                            stage,
                            run_idx,
                            artifacts.len(),
                            summary.join("\n")
                        );
                    }
                }
            }
            per_run_hashes.push(hash_artifacts(worktree.path(), &artifacts)?);
            // Copy every artifact to a per-run dump directory under the
            // report's parent. This is the diagnostic escape hatch:
            // when drift is detected, the full binaries are uploaded
            // alongside the JSON report so root-causing residual
            // non-determinism doesn't depend on re-running the harness.
            // Non-drifted entries are pruned after the comparison
            // below so the artifact zip stays compact.
            if let Some(parent) = self.report_path.parent() {
                let dump_root = parent.join("drift-bins").join(format!("run-{}", run_idx));
                copy_artifacts_to_dump(worktree.path(), &artifacts, &dump_root).with_context(
                    || {
                        format!(
                            "dumping artifacts to {} for determinism run {}",
                            dump_root.display(),
                            run_idx
                        )
                    },
                )?;
            }
            // Worktree dropped at end of scope → cleanup automatic.
        }

        let report = self.build_report(per_run_hashes);
        if let Some(parent) = self.report_path.parent() {
            prune_dump_to_drifted(&parent.join("drift-bins"), &report);
        }
        Ok(report)
    }

    /// Construct the env map handed to each child build process. See the
    /// module doc for the policy summary.
    fn build_isolated_env(
        &self,
        worktree: &Worktree,
        signing_keys: Option<&EphemeralSigningKeys>,
    ) -> Result<HashMap<String, String>> {
        let tmpdir = worktree.path().join(".det-tmp");
        std::fs::create_dir_all(&tmpdir)?;
        let cargo_home = tmpdir.join("cargo");
        let cargo_target = tmpdir.join("target");
        let home_dir = tmpdir.join("home");
        std::fs::create_dir_all(&cargo_home)?;
        std::fs::create_dir_all(&home_dir)?;

        Ok(build_subprocess_env(&BuildSubprocessEnv {
            cargo_home: &cargo_home,
            cargo_target: &cargo_target,
            tmpdir: &tmpdir,
            home_dir: &home_dir,
            sde: self.sde,
            worktree: worktree.path(),
            signing_keys,
        }))
    }

    /// Shell to the running `anodize` binary inside the worktree.
    ///
    /// Delegates to [`anodizer_core::determinism_runner`] — `crates/cli/**`
    /// is on the forbid-list in `.claude/rules/module-boundaries.md`, so
    /// the actual `Command::new` lives in core where subprocess spawn is
    /// allow-listed.
    ///
    /// The `--skip=<list>` argument is load-bearing — it pins the harness
    /// to the stages the operator actually asked to validate. Two
    /// independent contributions to the list:
    ///
    /// 1. [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`] —
    ///    upstream-touching stages that have no place in a hermetic
    ///    rebuild (publish, release, announce, blob, docker, ...). Lives
    ///    in core as the single source of truth so adding a new
    ///    side-effect stage to `pipeline.rs` MUST register it there.
    /// 2. [`compute_extra_skip`] — every other produce-stage the
    ///    operator did NOT request via `--stages=` (and that isn't in
    ///    [`PRESERVE_SET`]). Without this, the child release subprocess
    ///    would still try to run `nfpm` / `nsis` / `msi` / `dmg` / etc.
    ///    on shards that have no business running them, dying with `No
    ///    such file or directory` on missing tooling.
    fn run_build_pipeline(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let exe = anodizer_core::determinism_runner::current_anodize_binary()?;
        let extra_skip = compute_extra_skip(&self.stages);
        anodizer_core::determinism_runner::run_build_pipeline_subprocess(
            &exe,
            worktree_path,
            env,
            self.targets.as_deref(),
            &extra_skip,
        )
    }

    /// Aggregate per-run hashes into the final report.
    fn build_report(
        &self,
        per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>>,
    ) -> DeterminismReport {
        // Union of artifact names across runs — an artifact missing from
        // one run is itself a form of drift, surfaced as the run's hash
        // becoming `<missing>`.
        let mut all_names: BTreeSet<String> = BTreeSet::new();
        for run in &per_run_hashes {
            for name in run.keys() {
                all_names.insert(name.clone());
            }
        }

        let mut artifacts: Vec<ArtifactRow> = Vec::new();
        let mut drift: Vec<DriftRow> = Vec::new();
        let mut drift_count: u32 = 0;

        for name in &all_names {
            let mut hashes: Vec<String> = Vec::with_capacity(per_run_hashes.len());
            // Use the LAST run that produced the artifact as the source
            // of truth for path/size (matches the spec's "last writer
            // wins" semantics for the cosmetic fields).
            let mut last_info: Option<&ArtifactInfo> = None;
            for run in &per_run_hashes {
                match run.get(name) {
                    Some(info) => {
                        hashes.push(info.hash.clone());
                        last_info = Some(info);
                    }
                    None => hashes.push("<missing>".into()),
                }
            }

            let info = last_info.expect("artifact name came from union of run maps");
            let all_equal =
                hashes.iter().all(|h| h == &hashes[0]) && !hashes.iter().any(|h| h == "<missing>");
            // Sign-stage drift auto-allowlist: cosign sign-blob uses
            // ECDSA P-256 with a random nonce, so its signature bytes
            // can never be byte-identical across runs. Byte-equality is
            // not the right determinism signal for signatures —
            // verification (`cosign verify-blob` / `gpg --verify`) is.
            // The harness still records the per-run hashes in the drift
            // row for audit; the `<missing>` check above remains the
            // gate that catches a sign stage failing to produce output.
            let signed_artifact_drift = !all_equal && info.stage == "sign";
            let allow_reason = self.resolve_allow_reason(name).or_else(|| {
                if signed_artifact_drift {
                    Some(
                        "signed artifact: signature bytes vary by signer \
                         (cosign ECDSA random nonce); validate via \
                         `cosign verify-blob` / `gpg --verify`"
                            .into(),
                    )
                } else {
                    None
                }
            });

            if all_equal {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: true,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: Some(hashes[0].clone()),
                    hashes: vec![],
                });
            } else {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: false,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: None,
                    hashes: hashes.clone(),
                });
                // Drift row + drift_count are gated on allow-list status:
                // allow-listed artifacts surface their per-run hashes via
                // the drift row (so the audit trail is complete) but DO
                // NOT bump `drift_count`. See spec §"Verification harness
                // behavior".
                if allow_reason.is_none() {
                    // Diagnostic: scan the per-run head samples for the
                    // first differing byte offset. Without this, every
                    // drift cycle is blind — operators see "20 artifacts
                    // drift" but cannot tell whether the bytes are in
                    // the PE header, the gzip mtime, the SBOM UUID, or
                    // padding. The summary points the next fix-cycle at
                    // the right region in O(N) compute (vs an external
                    // hex-dump diff round-trip).
                    let summary = summarize_drift(name, &per_run_hashes);
                    let head_samples_b64 = per_run_hashes
                        .iter()
                        .filter_map(|run_hashes| run_hashes.get(name))
                        .map(|info| {
                            use base64::Engine as _;
                            base64::engine::general_purpose::STANDARD.encode(&info.head_sample)
                        })
                        .collect();
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: summary,
                        head_samples_b64,
                    });
                    drift_count += 1;
                }
            }
        }

        DeterminismReport {
            schema_version: CURRENT_SCHEMA_VERSION,
            anodize_version: env!("CARGO_PKG_VERSION").into(),
            commit: self.commit.clone(),
            commit_timestamp: self.sde,
            runs: self.runs,
            stages_under_test: self.stages.iter().map(|s| s.as_str().into()).collect(),
            allowlist: self.allowlist.clone(),
            artifacts,
            drift,
            drift_count,
        }
    }

    /// Match `artifact_name` against the harness allow-list. Compile-time
    /// entries win on collision per the
    /// [`anodizer_core::DeterminismState`] precedence rule.
    fn resolve_allow_reason(&self, artifact_name: &str) -> Option<String> {
        for entry in &self.allowlist.compile_time {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        for entry in &self.allowlist.runtime {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        None
    }
}

/// Explicit allow-list of host env vars the harness propagates into the
/// child build subprocess.
///
/// Two policy goals:
///
/// 1. **No credentials leak through.** Earlier shape was
///    `k.starts_with("GITHUB_")`, which would inherit `GITHUB_TOKEN` (the
///    OAuth token) and any future `GITHUB_PASSWORD`-style sibling. The
///    harness skips every token-consuming stage today, so the leak is
///    latent — but a future stage added to the build phase (e.g. a
///    registry-prefetch step) would silently acquire network creds inside
///    a supposedly-hermetic build. Audit reference:
///    `.claude/audits/2026-05-15-release-resilience-review.md#i7`.
///
/// 2. **Identity, not credentials.** Each entry below is either an
///    informational field (`GITHUB_REPOSITORY`, `RUNNER_OS`) or a build-
///    script identity input (`GITHUB_SHA`, `GITHUB_REF`). Nothing here
///    grants the child process network reach.
///
/// Notably *excluded*:
///
/// - `GITHUB_TOKEN`, `ACTIONS_RUNTIME_TOKEN`, `ACTIONS_CACHE_URL`,
///   `ACTIONS_*` — credential surface.
/// - `RUNNER_TEMP` — the harness already pins `TMPDIR` to a per-run path,
///   and `RUNNER_TEMP` would point outside the worktree.
///
/// Adding a new var here must be justified as identity-only; cred-bearing
/// vars belong in `crates/core/src/user_command.rs`'s sandboxed env
/// whitelist, not the harness's inheritance set.
///
/// This list is the contractual surface on **all** platforms. On Windows,
/// [`build_subprocess_env`] additionally inherits the rest of the host env
/// minus everything covered by [`windows_env_should_drop`] — credentials,
/// workflow-internal state, AND a hermeticity sweep that drops any
/// `GITHUB_*` / `RUNNER_*` name not present on this allow-list (those
/// namespaces carry host workflow state like `RUNNER_TEMP` /
/// `GITHUB_WORKSPACE` that would leak runner-owned paths into the
/// hermetic child). The MSVC toolchain block alone spans dozens of vars
/// that an allow-list cannot reasonably enumerate; the inverse approach
/// is the auditable contract — every entry in the skip set is either a
/// known credential carrier or a known hermeticity hazard.
const HARNESS_ENV_ALLOWLIST: &[&str] = &[
    // Toolchain identity.
    "RUSTUP_HOME",
    // CI signal (overridden to "true" below if unset).
    "CI",
    // GitHub Actions identity vars — owner/repo, commit, refs, run #.
    "GITHUB_REPOSITORY",
    "GITHUB_SHA",
    "GITHUB_REF",
    "GITHUB_REF_NAME",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_NUMBER",
    "GITHUB_WORKFLOW",
    "GITHUB_ACTOR",
    // Runner identity — OS / arch / hostname for build-script `cfg!()`.
    "RUNNER_OS",
    "RUNNER_ARCH",
    "RUNNER_NAME",
];

/// Credential-bearing env vars the Windows inherit-everything pass MUST
/// drop. The explicit list is the contractual surface; the
/// [`windows_env_should_drop`] suffix sweep is the defense-in-depth net
/// for vars not named here.
///
/// Membership policy: any var whose value grants network reach, signing
/// authority, or store-publishing rights. Workflow-internal vars that
/// aren't strictly credentials but pollute the child env
/// (`ACTIONS_RUNTIME_TOKEN`, `ACTIONS_CACHE_URL`, etc.) are also dropped
/// by name-pattern matching in [`windows_env_should_drop`].
#[cfg(windows)]
const WINDOWS_ENV_DENYLIST: &[&str] = &[
    // GitHub Actions / generic Git credentials.
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_PAT",
    // Cargo / publish credentials.
    "CARGO_REGISTRY_TOKEN",
    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
    // Cloud / store credentials.
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GCP_SERVICE_ACCOUNT_KEY",
    "AZURE_CLIENT_SECRET",
    // Anodize-publisher credentials.
    "CHOCOLATEY_API_KEY",
    "DOCKER_TOKEN",
    "DOCKERHUB_TOKEN",
    "GPG_PRIVATE_KEY",
    "GPG_PASSPHRASE",
    "COSIGN_KEY",
    "COSIGN_PASSWORD",
    "SNAPCRAFT_STORE_CREDENTIALS",
    "CLOUDSMITH_TOKEN",
    "MCP_GITHUB_TOKEN",
    "SMTP_PASSWORD",
    "ARTIFACTORY_TOKEN",
    "APK_PRIVATE_KEY",
    // Runner workflow-internal (drops also caught by ACTIONS_* prefix
    // and RUNNER_TOKEN exact-match in `windows_env_should_drop`; listed
    // here for explicit documentation).
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_RUNTIME_URL",
    "ACTIONS_CACHE_URL",
    "ACTIONS_RESULTS_URL",
    "RUNNER_TOKEN",
];

/// True when `key` names an env var the Windows inherit-everything pass
/// must drop. The predicate covers two distinct contracts:
///
/// 1. **Credentials / workflow-internal state** — vars whose value grants
///    network reach, signing authority, or store-publishing rights, plus
///    GH Actions workflow internals that pollute the child env.
/// 2. **Hermeticity** — vars in the `GITHUB_*` / `RUNNER_*` namespaces that
///    are NOT on [`HARNESS_ENV_ALLOWLIST`]. The allow-list captures the
///    identity-only subset (repo / sha / refs / run #, os / arch /
///    hostname); the rest of those namespaces is host workflow state
///    (`RUNNER_TEMP`, `RUNNER_TOOL_CACHE`, `RUNNER_WORKSPACE`,
///    `GITHUB_WORKSPACE`, `GITHUB_EVENT_PATH`, ...) — path-pointing or
///    workflow-state values that would leak the GH Actions runner's
///    on-host directories into the supposedly hermetic child, breaking
///    determinism (cargo / cc-rs / Win32 would see host paths that aren't
///    isolated). Audit reference: I7 in
///    `.claude/audits/2026-05-15-release-resilience-review.md` — the
///    original design explicitly excluded `RUNNER_TEMP` for this reason;
///    the inverse-by-deny-list redesign for Windows had to broaden the
///    skip rule to cover the whole namespace.
///
/// Check order: explicit deny-list → ACTIONS_* / RUNNER_TOKEN → credential
/// suffix sweep → GH/RUNNER namespace hermeticity gate.
#[cfg(windows)]
fn windows_env_should_drop(key: &str) -> bool {
    if WINDOWS_ENV_DENYLIST
        .iter()
        .any(|d| d.eq_ignore_ascii_case(key))
    {
        return true;
    }
    if key.starts_with("ACTIONS_") || key.eq_ignore_ascii_case("RUNNER_TOKEN") {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    for suffix in [
        "_token",
        "_key",
        "_secret",
        "_password",
        "_passphrase",
        "_credentials",
    ] {
        if lower.ends_with(suffix) {
            return true;
        }
    }
    // Hermeticity sweep: GH/RUNNER namespace vars not on the
    // identity-only allow-list are host workflow state and must not
    // propagate into the hermetic child. The allow-list pass earlier in
    // `build_subprocess_env` already re-populated the identity subset
    // from the host env, so dropping the rest here doesn't lose anything
    // we still want.
    if key.starts_with("GITHUB_") || key.starts_with("RUNNER_") {
        let in_allowlist = HARNESS_ENV_ALLOWLIST
            .iter()
            .any(|a| a.eq_ignore_ascii_case(key));
        if !in_allowlist {
            return true;
        }
    }
    false
}

/// Inputs for [`build_subprocess_env`]. Bundled so the function signature
/// doesn't grow more positional arguments every time we add an isolated-
/// path knob.
pub(crate) struct BuildSubprocessEnv<'a> {
    pub cargo_home: &'a Path,
    pub cargo_target: &'a Path,
    pub tmpdir: &'a Path,
    pub home_dir: &'a Path,
    pub sde: i64,
    /// Absolute path to the per-run worktree root. Used to inject
    /// `RUSTFLAGS=--remap-path-prefix=<worktree>=/anodize` into the child
    /// build subprocess so two harness runs (at different worktree paths)
    /// produce a byte-identical anodizer binary. Without this remap,
    /// rustc embeds the absolute worktree path into `panic!()` location
    /// strings (`file!()`, `Location::caller()`), driving binary drift
    /// that then propagates into every archive that wraps the binary.
    pub worktree: &'a Path,
    /// Ephemeral signing keys for the sign stage. `None` skips the
    /// keying env-var block (caller is opting out of sign-stage
    /// validation). When `Some`, the harness exports `COSIGN_KEY` /
    /// `COSIGN_PASSWORD` / `GNUPGHOME` / `GPG_FINGERPRINT` / `GPG_TTY` /
    /// `GPG_KEY_PATH` / `ANODIZER_IN_DETERMINISM_HARNESS=1` into the
    /// child env. The same `signing_keys` reference is passed to every
    /// run so both rebuilds sign with identical key material.
    pub signing_keys: Option<&'a EphemeralSigningKeys>,
}

/// Pure constructor for the child env map. Factored out of
/// [`Harness::build_isolated_env`] so unit tests can drive the env-shape
/// logic without standing up a real worktree on disk.
///
/// Reads from `std::env::vars()` for the allow-listed identity vars (see
/// [`HARNESS_ENV_ALLOWLIST`]). Unit tests that care about the host-env
/// pass-through must serialize on the `harness_env` lock group via
/// `serial_test::serial(harness_env)` — env vars are process-global state
/// and parallel tests racing on the same key cause flakes.
pub(crate) fn build_subprocess_env(inputs: &BuildSubprocessEnv<'_>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "CARGO_HOME".into(),
        inputs.cargo_home.to_string_lossy().into_owned(),
    );
    env.insert(
        "CARGO_TARGET_DIR".into(),
        inputs.cargo_target.to_string_lossy().into_owned(),
    );
    env.insert(
        "TMPDIR".into(),
        inputs.tmpdir.to_string_lossy().into_owned(),
    );
    env.insert(
        "HOME".into(),
        inputs.home_dir.to_string_lossy().into_owned(),
    );
    env.insert("SOURCE_DATE_EPOCH".into(), inputs.sde.to_string());
    env.insert("PATH".into(), allow_listed_path());

    // Inject `--remap-path-prefix` so the absolute worktree path doesn't
    // leak into the compiled binary. Each run uses a different worktree
    // (`/tmp/anodize-determinism-<pid>-<idx>`), and rustc embeds the
    // absolute workspace path into every `file!()` / `Location::caller()`
    // expansion (panic location strings, `#[track_caller]` slots,
    // line-tables-only debug info). Two runs at different paths therefore
    // produce binaries that differ at the byte locations where those
    // strings live — even though the source code is identical. The
    // archive stage then wraps the (drifted) binary into tar.gz /
    // tar.xz / tar.zst, propagating the drift into every archive
    // format with identical total size (the path string length is
    // constant). Remapping the worktree to a stable sentinel
    // (`/anodize`) collapses both runs onto the same byte sequence.
    //
    // Also remap CARGO_HOME (the child's per-run cargo home, which lives
    // under tmpdir) and CARGO_TARGET_DIR for the same reason: registry
    // dependency paths and incremental compilation artifacts can
    // surface in panic strings via inlined helpers from std / proc
    // macros. Cargo auto-applies a similar remap for CARGO_HOME on
    // recent stable, but mirroring it explicitly is defense-in-depth
    // against version skew.
    //
    // We append our remap entries to any host-supplied RUSTFLAGS rather
    // than overwriting them: cargo otherwise reads the env var verbatim,
    // and an operator who set RUSTFLAGS for cross-compile linker flags
    // (e.g. `-C linker=<wrapper>`) would silently lose them. Append
    // semantics also match how `stage-build` layers its own remap when
    // `reproducible: true`.
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let worktree_str = inputs.worktree.to_string_lossy();
    let cargo_home_str = inputs.cargo_home.to_string_lossy();
    let cargo_target_str = inputs.cargo_target.to_string_lossy();
    for (from, to) in [
        (worktree_str.as_ref(), "/anodize"),
        (cargo_home_str.as_ref(), "/cargo"),
        (cargo_target_str.as_ref(), "/target"),
    ] {
        if from.is_empty() {
            continue;
        }
        let flag = format!("--remap-path-prefix={}={}", from, to);
        if !rustflags.is_empty() {
            rustflags.push(' ');
        }
        rustflags.push_str(&flag);
    }
    if !rustflags.is_empty() {
        env.insert("RUSTFLAGS".into(), rustflags.clone());
    }

    // Windows MSVC determinism flags. See the [target.*] rustflags in
    // `.cargo/config.toml` for the non-harness path. This per-target
    // env var path is required because the harness sets RUSTFLAGS for
    // `--remap-path-prefix`, which (per cargo precedence) suppresses
    // the `[target.<triple>] rustflags` config entry; the flags must
    // be re-applied via `CARGO_TARGET_<triple>_RUSTFLAGS` so the
    // MSVC build gets both the path remap and the MSVC-specific flags.
    //
    // The flag set:
    //   - `-C codegen-units=1` — single codegen unit so cross-CU
    //     function-ordering non-determinism doesn't shuffle the
    //     resulting object's symbol/section layout. Drives Data
    //     Directory RVA stability at PE offset 0x108+.
    //   - `-C link-arg=/Brepro` — substitute PE `TimeDateStamp` with a
    //     content hash. <https://learn.microsoft.com/en-us/cpp/build/reference/brepro>
    //   - `-C link-arg=/OPT:NOICF` — disable Identical COMDAT Folding.
    //     ICF's fold decisions depend on input-file presentation order
    //     and can shuffle which symbol resolves to which fold target.
    //   - `-C link-arg=/INCREMENTAL:NO` — disable incremental linking
    //     (release profile default, but explicit because `/Brepro` is
    //     incompatible with incremental linking and a future profile
    //     edit shouldn't silently re-enable it).
    //   - `-C link-arg=/PDBALTPATH:%_PDB%` — embed only the PDB
    //     filename (no absolute path) in the binary's debug directory.
    //     Otherwise the per-run worktree path embeds into the binary
    //     and drifts even with `/Brepro` content-hashing.
    //   - `-C link-arg=/DEBUG:NONE` — do not emit PDB or CodeView
    //     records. Even with `/PDBALTPATH`, the linker still embeds a
    //     CV_INFO_PDB70 record whose GUID is derived non-deterministically
    //     from the build session. Eliminating the debug directory is the
    //     reliable cure; production debug symbols are not generally
    //     needed for a CLI binary.
    let msvc_flags = [
        "-C codegen-units=1",
        "-C link-arg=/Brepro",
        "-C link-arg=/OPT:NOICF",
        "-C link-arg=/INCREMENTAL:NO",
        "-C link-arg=/PDBALTPATH:%_PDB%",
        "-C link-arg=/DEBUG:NONE",
        "-C strip=symbols",
    ];
    for triple in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let mut per_target = rustflags.clone();
        for flag in msvc_flags {
            if !per_target.is_empty() {
                per_target.push(' ');
            }
            per_target.push_str(flag);
        }
        let key = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            triple.replace('-', "_").to_uppercase()
        );
        env.insert(key, per_target);
    }

    // On Windows, the host build (e.g. `cargo run --release` invoked
    // by a `before:` hook) lands at `target/release/anodizer.exe`.
    // Cargo's host build reads global `RUSTFLAGS` (not the per-target
    // `CARGO_TARGET_<HOST>_RUSTFLAGS`, which only applies when
    // `--target=<HOST>` is explicit). Append the MSVC flag set to
    // global RUSTFLAGS too so the host build is also reproducible.
    // Safe on Windows runners because the host triple IS msvc, so the
    // link.exe-specific flags are valid for every build (proc-macros,
    // build scripts, etc.).
    if cfg!(windows) {
        for flag in msvc_flags {
            if !rustflags.is_empty() {
                rustflags.push(' ');
            }
            rustflags.push_str(flag);
        }
        env.insert("RUSTFLAGS".into(), rustflags.clone());
    }

    // Inherit only the explicit allow-list of identity-only host env so
    // build scripts that conditionally embed git/CI info still work, and
    // no credential-bearing vars (GITHUB_TOKEN, ACTIONS_RUNTIME_TOKEN,
    // etc.) leak into the child.
    for &key in HARNESS_ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    // Windows env is sprawling; cc-rs / cargo / rustc rely on
    // PROGRAMFILES*, WINDIR, SystemRoot, PROCESSOR_*, USERPROFILE,
    // APPDATA, LOCALAPPDATA, TEMP, TMP, PATHEXT, and the entire MSVC
    // toolchain block (VC* / VS* / INCLUDE / LIB / LIBPATH / WindowsSdk*
    // / UCRT* / Platform). Enumerating each in the allow-list is fragile
    // and discovery-driven. Instead: inherit everything from the host
    // env and drop the credential deny-list, a suffix sweep, and the
    // GH/RUNNER hermeticity sweep (see [`windows_env_should_drop`]).
    // The allow-list pass above still ran first; this loop adds the
    // rest. `or_insert` preserves any value already set (the child's
    // CARGO_HOME / HOME / TMPDIR overrides above survive even if the
    // host carries them).
    #[cfg(windows)]
    for (key, value) in std::env::vars() {
        if windows_env_should_drop(&key) {
            continue;
        }
        env.entry(key).or_insert(value);
    }
    // rustup needs RUSTUP_HOME to dispatch a toolchain; on GH Actions
    // runners (and most dev machines) it isn't set in the env — rustup
    // defaults to $HOME/.rustup. Since the child runs with HOME=tmpdir,
    // we must compute the default from the HOST's HOME (Unix) or
    // USERPROFILE (Windows) and propagate it explicitly. Uses
    // `or_insert_with` so a host RUSTUP_HOME (inherited via the
    // allow-list above) takes precedence over the synthesized default.
    env.entry("RUSTUP_HOME".into()).or_insert_with(|| {
        let host_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        host_home.join(".rustup").to_string_lossy().into_owned()
    });
    // Always set CI=true so build scripts know they're in a sealed env.
    env.entry("CI".into()).or_insert_with(|| "true".into());

    // Inserted last so the harness's ephemeral material wins over any
    // host-leaked credential vars under either the allow-list or the
    // Windows inherit pass.
    if let Some(keys) = inputs.signing_keys {
        env.insert("ANODIZER_IN_DETERMINISM_HARNESS".into(), "1".into());
        env.insert("COSIGN_KEY".into(), keys.cosign_key_contents.clone());
        env.insert("COSIGN_PASSWORD".into(), keys.cosign_password.clone());
        env.insert(
            "GNUPGHOME".into(),
            anodizer_core::harness_signing::path_for_subprocess_env(&keys.gnupg_home),
        );
        env.insert("GPG_FINGERPRINT".into(), keys.gpg_fingerprint.clone());
        env.insert("GPG_TTY".into(), "/dev/null".into());
        env.insert(
            "GPG_KEY_PATH".into(),
            anodizer_core::harness_signing::path_for_subprocess_env(&keys.gpg_key_path),
        );
    }

    env
}

/// Per-run artifact info captured by `hash_artifacts`. Internal-only.
#[derive(Debug, Clone)]
struct ArtifactInfo {
    hash: String,
    size_bytes: u64,
    /// Path relative to the worktree root (with leading `dist/` etc).
    /// Used as the canonical `ArtifactRow.path` value.
    relative_path: String,
    /// Best-effort stage attribution from the path prefix.
    stage: String,
    /// First [`HEAD_SAMPLE_BYTES`] bytes of the artifact, retained so
    /// the harness can populate `DriftRow.differing_bytes_summary`
    /// after the worktree is dropped. Why a head sample (not the full
    /// content): the largest artifact in the pipeline is the raw
    /// `.exe` at ~50 MB; multiplied by N runs and ~50 artifacts/run
    /// the retained bytes would blow past the report file's useful
    /// size. The head is what matters for PE / archive / Mach-O drift
    /// (their metadata is front-loaded), and the sample is read
    /// once during the existing `std::fs::read` so there's no extra
    /// I/O.
    head_sample: Vec<u8>,
    /// Last [`TAIL_SAMPLE_BYTES`] bytes of the artifact. Complements
    /// `head_sample`: trailing structures that drift past 1 KiB —
    /// gzip footer (`mtime`, ISIZE), zstd skippable frames, ZIP
    /// central directory, PE Debug Directory contents, detached
    /// signature `.sig` trailers — get a localized offset instead of
    /// `"no diff in first 1 KiB"`. Empty when the artifact is smaller
    /// than the head window (the head already covers the whole file).
    tail_sample: Vec<u8>,
}

/// How many leading bytes of each artifact to retain for drift
/// diagnostics. 1 KiB covers:
///   - PE: DOS stub + PE signature + COFF header + Optional header +
///     first ~10 section table entries (each entry is 40 B). Catches
///     `TimeDateStamp`, `MajorLinkerVersion`, debug directory RVA, and
///     the start of the Rich header.
///   - tar.gz: gzip header (10 B) + first tar entry header (512 B) +
///     500 B of the first file's data. Catches gzip `mtime` and tar
///     `mtime` drift.
///   - zip: local file header + filename + first file's data start.
///     Catches `mod_time` / `mod_date` drift.
///   - CycloneDX SBOM JSON: top-level keys including
///     `serialNumber` (per-run UUID — a known drift source).
const HEAD_SAMPLE_BYTES: usize = 16 * 1024;

/// How many trailing bytes of each artifact to retain alongside the
/// head sample. Catches trailing-section drift that the head misses:
///   - gzip footer: 4-byte `mtime` + 4-byte ISIZE.
///   - zstd: skippable frames + content checksum (last 4 B).
///   - ZIP: central directory record + end-of-central-directory
///     record (`EOCD`) including the per-archive comment.
///   - PE: Debug Directory contents (GUID + age + PDB path), import
///     address table, resource section drift.
///   - Detached signatures (`.sig`): cosign/gpg signature blob lives
///     entirely past the head window.
const TAIL_SAMPLE_BYTES: usize = 16 * 1024;

/// PATH for harness children — inherits the host's PATH verbatim on
/// every platform.
///
/// The harness's hermeticity goal is to isolate cargo/build outputs
/// from the host's CARGO_HOME and HOME (so two runs of the same commit
/// don't share warm caches), NOT to tighten the binary-search path.
/// Two runs from the same host process see identical host PATH, so
/// determinism is preserved. Inheriting host PATH also makes the
/// harness work uniformly on Windows (git at `C:\Program Files\Git\cmd`),
/// macOS Apple Silicon (`/opt/homebrew/bin`), and Linux (`/usr/bin`,
/// `~/.cargo/bin`) without per-platform allow-list maintenance.
///
/// **Windows note**: the deny-list inherit-everything pass in
/// [`build_subprocess_env`] propagates the full MSVC toolchain block
/// (`VC*` / `VS*` / `INCLUDE` / `LIB` / `LIBPATH` / `WindowsSdk*` /
/// `UCRT*`) from the host. cc-rs uses those env vars (and a registry
/// lookup) to resolve MSVC's `link.exe` to an *absolute* path before
/// invoking rustc with `-Clinker=<abs>`, so PATH ordering ambiguity
/// between MSVC's `link.exe` (the linker) and Git for Windows's GNU
/// `link.exe` (the hardlink tool) is moot — rustc never falls back to a
/// bare PATH lookup. No PATH filtering is needed.
fn allow_listed_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Walk `<worktree>/dist` and collect every regular file. Sorted by path
/// for deterministic iteration order in tests.
///
/// Also surfaces the **raw cargo build outputs** at
/// `<worktree>/.det-tmp/target/<triple>/release/<bin>` (or
/// `<worktree>/.det-tmp/target/release/<bin>` when the build wasn't
/// `--target`-pinned). These are the SOURCE of any RUSTFLAGS / mtime /
/// build-script drift that later propagates into every wrapped archive
/// (`.tar.gz`, `.tar.xz`, `.zip`, ...). Hashing them directly lets the
/// report point a finger at the raw binary instead of the operator
/// having to peel six layers of containers to find that the underlying
/// `target/release/anodize` was nondeterministic. Path-remapping
/// (`--remap-path-prefix`) is already applied via the env block, so on
/// a healthy run these hashes will match; if they ever drift, we want
/// the diagnostic chain to start here.
///
/// The function only walks the immediate `release/` directory (not
/// `deps/`, `build/`, `.fingerprint/`, etc.) and filters to files
/// without an extension or with `.exe` — anodize ships single-binary
/// crates, so this surfaces the actual `anodize` / `anodize.exe`
/// without dragging in cargo's incremental-build scratch.
fn discover_artifacts(worktree_path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let dist = worktree_path.join("dist");
    if dist.exists() {
        visit_dir(&dist, &mut out)?;
    }

    let target_root = worktree_path.join(".det-tmp").join("target");
    if target_root.exists() {
        collect_raw_binaries(&target_root, &mut out)?;
    }

    out.sort();
    Ok(out)
}

fn visit_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            visit_dir(&entry.path(), out)?;
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Collect raw cargo release binaries from `<cargo_target>/[<triple>/]release/`.
///
/// Two layouts to support:
///
/// - `<cargo_target>/release/<bin>` — host build, no `--target` flag.
/// - `<cargo_target>/<triple>/release/<bin>` — cross-target build.
///
/// We only emit the top-level files inside each `release/` directory.
/// `release/deps`, `release/build`, `release/.fingerprint`, etc. are
/// cargo's internal scratch and not what we want to fingerprint for
/// drift detection.
///
/// File filter: regular files whose extension is empty (`anodize`) or
/// `.exe` (`anodize.exe`). Excludes `.d` (depfiles), `.pdb` (debug
/// symbols), `.rlib`, etc. — those are tooling byproducts, not the
/// shippable binary that lands in archives.
fn collect_raw_binaries(target_root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    // Top-level entries under <cargo_target>/. Each is either a triple
    // directory (cross build) OR `release` / `debug` (host build).
    let entries = match std::fs::read_dir(target_root) {
        Ok(e) => e,
        // Race / cleanup raced us — treat as empty.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target_root.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name_s == "release" {
            // Host build, no --target.
            push_release_dir_files(&entry.path(), out)?;
        } else if name_s == "debug"
            || name_s == ".rustc_info.json"
            || name_s == "CACHEDIR.TAG"
            || name_s.starts_with('.')
        {
            // Skip debug builds and cargo metadata.
            continue;
        } else {
            // Treat as a target triple — look for <triple>/release/.
            let release_dir = entry.path().join("release");
            if release_dir.is_dir() {
                push_release_dir_files(&release_dir, out)?;
            }
        }
    }
    Ok(())
}

fn push_release_dir_files(release_dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(release_dir)
        .with_context(|| format!("reading {}", release_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            // deps/, build/, .fingerprint/, examples/ — cargo scratch,
            // not the shippable binary.
            continue;
        }
        let path = entry.path();
        match path.extension().and_then(|s| s.to_str()) {
            // anodize binary (Unix: no extension) — include.
            None => out.push(path),
            // anodize.exe (Windows) — include.
            Some("exe") => out.push(path),
            // .d depfiles, .pdb debug symbols, .rlib, .rmeta, etc. —
            // cargo build byproducts, not what gets shipped.
            _ => continue,
        }
    }
    Ok(())
}

/// SHA256 every artifact and return `{name -> info}`.
///
/// Map keys are usually the artifact basename (matching the spec's
/// allow-list pattern semantics — glob/exact matches operate on the
/// file name). Raw cargo binaries under `<worktree>/.det-tmp/target`
/// get a `target/<triple>/<bin>` (or `target/release/<bin>` for host
/// builds) prefix so the report unambiguously distinguishes
/// `dist/anodize` (the shipped binary inside an archive) from
/// `target/<triple>/anodize` (the raw cargo output that flows INTO
/// the archive). Without the prefix, a reader of the report can't
/// tell which file's hash they're looking at when both kinds exist.
fn hash_artifacts(
    worktree_path: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<String, ArtifactInfo>> {
    use sha2::{Digest, Sha256};
    let mut out = BTreeMap::new();
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let bytes =
            std::fs::read(p).with_context(|| format!("reading artifact {}", p.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = format!("sha256:{:x}", hasher.finalize());
        let relative = p
            .strip_prefix(worktree_path)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned();
        let name = if let Ok(under_target) = p.strip_prefix(&target_root) {
            // Raw cargo binary: prefix with `target/` and the
            // <triple>/release/ (or release/) segments so the report
            // surfaces it distinctly from any `dist/` artifact of the
            // same basename. Forward slashes regardless of platform
            // (matches `Artifact::to_artifacts_json` normalization).
            let suffix = under_target.to_string_lossy().replace('\\', "/");
            format!("target/{}", suffix)
        } else {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let stage = infer_stage_from_path(&relative);
        let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
        let head_sample = bytes[..head_len].to_vec();
        // Tail sample is non-overlapping with the head: when the file
        // is smaller than HEAD + TAIL, the head already covers the
        // whole content and the tail is left empty so the drift
        // summary doesn't double-count bytes.
        let tail_sample = if bytes.len() > HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES {
            bytes[bytes.len() - TAIL_SAMPLE_BYTES..].to_vec()
        } else {
            Vec::new()
        };
        out.insert(
            name,
            ArtifactInfo {
                hash: digest,
                size_bytes: bytes.len() as u64,
                relative_path: relative,
                stage,
                head_sample,
                tail_sample,
            },
        );
    }
    Ok(out)
}

/// Copy each artifact in `paths` to `dump_root/<artifact-name>`,
/// preserving the relative directory structure under `worktree_path`.
///
/// Best-effort: copy failures are logged but not surfaced, so the
/// harness's primary determinism check is never broken by a side
/// channel diagnostic.
fn copy_artifacts_to_dump(worktree_path: &Path, paths: &[PathBuf], dump_root: &Path) -> Result<()> {
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let dest_rel = if let Ok(under_target) = p.strip_prefix(&target_root) {
            PathBuf::from("target").join(under_target)
        } else if let Ok(under_worktree) = p.strip_prefix(worktree_path) {
            under_worktree.to_path_buf()
        } else {
            PathBuf::from(p.file_name().unwrap_or_default())
        };
        let dest = dump_root.join(dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dump parent {}", parent.display()))?;
        }
        if let Err(e) = std::fs::copy(p, &dest) {
            eprintln!(
                "warn: drift-bin dump failed for {} -> {}: {}",
                p.display(),
                dest.display(),
                e
            );
        }
    }
    Ok(())
}

/// Prune `<dump_root>/run-<N>/<artifact>` entries whose artifact name
/// does NOT appear in `report.drift`. Keeps the artifact upload
/// compact (drifted binaries only) without sacrificing the per-run
/// dump that the harness captured pre-comparison.
fn prune_dump_to_drifted(dump_root: &Path, report: &DeterminismReport) {
    if !dump_root.exists() {
        return;
    }
    let drift_names: std::collections::HashSet<&str> =
        report.drift.iter().map(|d| d.artifact.as_str()).collect();
    let Ok(run_dirs) = std::fs::read_dir(dump_root) else {
        return;
    };
    for run_entry in run_dirs.flatten() {
        let run_path = run_entry.path();
        if !run_path.is_dir() {
            continue;
        }
        prune_dump_subtree(&run_path, &run_path, &drift_names);
    }
}

fn prune_dump_subtree(root: &Path, dir: &Path, drift_names: &std::collections::HashSet<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_dump_subtree(root, &path, drift_names);
            // Empty post-prune → remove the parent dir too.
            if std::fs::read_dir(&path)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&path);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !drift_names.contains(rel.as_str()) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Produce a one-line human-readable summary of where two runs' head
/// samples diverge for a given artifact. Returns `None` when fewer
/// than two runs have head-sample data for the artifact (the comparison
/// is meaningless with one data point — the surrounding `if all_equal`
/// already established that the hashes differed, so the missing-sample
/// path is purely a defensive guard, not the common case).
///
/// Output shapes (first applicable):
///   - `"first diff at offset 0xNN (run0=0xXX, run1=0xYY)"` — head
///     diverges within [`HEAD_SAMPLE_BYTES`].
///   - `"tail diff at -0xNN from end (size B, run0=0xXX, run1=0xYY)"` —
///     heads match but tails diverge; `-0xNN` is the offset from EOF.
///     Catches gzip/zip footer drift, signature trailers, PE Debug
///     Directory drift.
///   - `"no diff in first/last K bytes; sizes differ..."` — both
///     sampled windows match but total sizes differ: drift is in the
///     un-sampled middle.
///
/// For three-or-more-run reports (currently the harness defaults to 2
/// but `--runs=N` is a CLI flag), the summary compares run0 vs the
/// first differing run; if all subsequent runs also diverge from run0,
/// reporting the first divergence is sufficient to localize the source.
fn summarize_drift(
    name: &str,
    per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
) -> Option<String> {
    let samples: Vec<(&[u8], &[u8], u64)> = per_run_hashes
        .iter()
        .filter_map(|run| {
            run.get(name).map(|info| {
                (
                    info.head_sample.as_slice(),
                    info.tail_sample.as_slice(),
                    info.size_bytes,
                )
            })
        })
        .collect();
    if samples.len() < 2 {
        return None;
    }
    let (head0, tail0, size0) = samples[0];
    if let Some((idx, head_n, offset)) =
        samples
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(idx, &(head_n, _, _))| {
                let common = head0.len().min(head_n.len());
                (0..common)
                    .find(|&i| head0[i] != head_n[i])
                    .map(|off| (idx, head_n, off))
            })
    {
        return Some(format!(
            "first diff at offset {:#x} (run0={:#04x}, run{idx}={:#04x})",
            offset, head0[offset], head_n[offset]
        ));
    }
    if let Some((idx, head_n)) = samples
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(idx, &(head_n, _, _))| (head_n.len() != head0.len()).then_some((idx, head_n)))
    {
        return Some(format!(
            "head samples differ in length: run0={} bytes, run{idx}={} bytes",
            head0.len(),
            head_n.len()
        ));
    }
    // Heads match. Check the tail window for trailing-section drift.
    // Only meaningful when both runs captured a non-empty tail and
    // total sizes agree (otherwise the size-diff branch below is more
    // informative — different EOFs make tail offsets compare apples
    // to oranges).
    if !tail0.is_empty()
        && let Some((idx, tail_n, offset)) =
            samples
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(idx, &(_, tail_n, size_n))| {
                    if tail_n.is_empty() || size_n != size0 || tail_n.len() != tail0.len() {
                        return None;
                    }
                    (0..tail0.len())
                        .find(|&i| tail0[i] != tail_n[i])
                        .map(|off| (idx, tail_n, off))
                })
    {
        let from_end = tail0.len() - offset;
        return Some(format!(
            "tail diff at -{:#x} from end (size {}, run0={:#04x}, run{idx}={:#04x})",
            from_end, size0, tail0[offset], tail_n[offset]
        ));
    }
    if let Some((idx, size_n)) = samples
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(idx, &(_, _, size_n))| (size_n != size0).then_some((idx, size_n)))
    {
        return Some(format!(
            "no diff in first {} or last {} bytes; total size run0={} run{idx}={} \
             (drift in un-sampled middle)",
            head0.len(),
            tail0.len(),
            size0,
            size_n
        ));
    }
    Some(format!(
        "no diff in first {} or last {} bytes; sizes equal at {} bytes \
         (drift in un-sampled middle)",
        head0.len(),
        tail0.len(),
        size0
    ))
}

/// Best-effort stage attribution from the artifact path. The harness
/// does not have access to the pipeline's per-stage Artifact records (it
/// shells to a child process), so it infers from filename extension and
/// path conventions. Falls back to `"unknown"` when nothing matches.
fn infer_stage_from_path(rel: &str) -> String {
    // Normalize Windows backslashes so the contains/starts_with checks
    // below match regardless of host platform.
    let lower = rel.replace('\\', "/").to_lowercase();
    // Raw cargo build output under `<worktree>/.det-tmp/target/...` —
    // attribute to `build` so the report makes the source-of-drift
    // chain explicit (build → archive → checksum → sign).
    if lower.contains("/.det-tmp/target/") || lower.starts_with(".det-tmp/target/") {
        return "build".into();
    }
    if lower.ends_with(".sig") || lower.ends_with(".pem") || lower.ends_with(".cert") {
        "sign".into()
    } else if lower.contains("checksums")
        || lower.ends_with("sha256sum")
        || lower.ends_with("sha256sums")
        || lower.ends_with(".sha256")
    {
        "checksum".into()
    } else if lower.ends_with(".sbom.json")
        || lower.ends_with(".cdx.json")
        || lower.ends_with(".spdx.json")
    {
        "sbom".into()
    } else if lower.ends_with(".tar.gz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".tar.zst")
        || lower.ends_with(".zip")
        || lower.ends_with(".tar")
    {
        "archive".into()
    } else if lower.ends_with(".crate") {
        "cargo-package".into()
    } else {
        "unknown".into()
    }
}

/// Glob match copy-paste from `anodizer_core::determinism` (kept local
/// to avoid exposing that helper publicly; the determinism module owns
/// the canonical semantics). `*.ext` is suffix-match; anything else is
/// exact-match.
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

/// Pick the first artifact whose inferred stage matches `stage_name`,
/// in the sorted order returned by [`discover_artifacts`]. Returns
/// `None` when no artifact maps to the named stage (caller silently
/// no-ops — the integration test should observe drift_count == 0 in
/// that case, surfacing a typo in the stage value).
fn pick_first_artifact_for_stage<'a>(
    artifacts: &'a [PathBuf],
    stage_name: &str,
) -> Option<&'a PathBuf> {
    artifacts.iter().find(|p| {
        let rel = p.to_string_lossy();
        infer_stage_from_path(&rel) == stage_name
    })
}

/// Append one byte to `path` to force the artifact to differ across
/// runs. Used by the `--inject-drift=<stage>` test-harness flag.
///
/// Source byte: `/dev/urandom` on platforms that expose it;
/// `SystemTime::now().subsec_nanos()` fallback otherwise. The fallback
/// MUST vary between successive runs — when the underlying archive is
/// fully deterministic (the goal of this harness), appending a
/// CONSTANT byte to two byte-identical archives yields two
/// byte-identical archives and the harness reports no drift. The
/// nanos fallback varies on every call (successive harness runs are
/// at least milliseconds apart), so the appended byte differs across
/// runs and the hash diverges as intended. Reading from
/// `/dev/urandom` on Unix is the path actually used in CI for that
/// platform.
fn inject_drift_byte(path: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let byte: u8 = match std::fs::OpenOptions::new().read(true).open("/dev/urandom") {
        Ok(mut f) => {
            let mut buf = [0u8; 1];
            f.read_exact(&mut buf).ok();
            buf[0]
        }
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u8)
            .unwrap_or(0xAB),
    };
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    f.write_all(&[byte])
        .with_context(|| format!("appending drift byte to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::AllowListEntry;

    fn empty_harness() -> Harness {
        Harness {
            repo_root: PathBuf::from("/tmp/unused"),
            commit: "deadbeef".into(),
            stages: vec![StageId::Archive, StageId::Checksum],
            runs: 2,
            sde: 1_715_000_000,
            allowlist: AllowList::default(),
            report_path: PathBuf::from("/tmp/unused/report.json"),
            inject_drift: None,
            targets: None,
        }
    }

    fn run_with_files(
        h: &Harness,
        runs: Vec<Vec<(&str, &[u8])>>,
    ) -> Vec<BTreeMap<String, ArtifactInfo>> {
        // Synthesize per-run hash maps as if the child build pipeline
        // had emitted each file. Bypasses the actual subprocess so unit
        // tests don't depend on cargo / rustup / git.
        let _ = h;
        runs.into_iter()
            .map(|files| {
                let mut map = BTreeMap::new();
                for (name, bytes) in files {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(bytes);
                    let digest = format!("sha256:{:x}", hasher.finalize());
                    let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
                    let tail_sample = if bytes.len() > HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES {
                        bytes[bytes.len() - TAIL_SAMPLE_BYTES..].to_vec()
                    } else {
                        Vec::new()
                    };
                    map.insert(
                        name.into(),
                        ArtifactInfo {
                            hash: digest,
                            size_bytes: bytes.len() as u64,
                            relative_path: format!("dist/{}", name),
                            stage: infer_stage_from_path(name),
                            head_sample: bytes[..head_len].to_vec(),
                            tail_sample,
                        },
                    );
                }
                map
            })
            .collect()
    }

    #[test]
    fn harness_report_shape_serializes_correctly() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("anodizer_0.2.1.tar.gz", b"hello")],
                vec![("anodizer_0.2.1.tar.gz", b"hello")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.runs, 2);
        assert_eq!(report.commit, "deadbeef");
        assert_eq!(report.stages_under_test, vec!["archive", "checksum"]);
        assert_eq!(report.drift_count, 0);
        assert_eq!(report.artifacts.len(), 1);
        assert!(report.artifacts[0].deterministic);
        assert!(report.artifacts[0].hash.is_some());
        assert!(report.artifacts[0].hashes.is_empty());

        // Round-trip JSON.
        let s = serde_json::to_string_pretty(&report).unwrap();
        let back: DeterminismReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn harness_diffs_artifacts_by_sha256() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"first")],
                vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"second")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.drift.len(), 1);
        assert_eq!(report.drift[0].artifact, "drifting.tar.gz");
        assert_eq!(report.drift[0].hashes.len(), 2);
        assert_ne!(report.drift[0].hashes[0], report.drift[0].hashes[1]);
        // Diagnostic: the drift row must carry a `differing_bytes_summary`
        // so future fix-cycles aren't blind. "first" and "second" diverge
        // at byte 0 (0x66 'f' vs 0x73 's'), so the summary should call
        // out offset 0x0. Without this, every drift cycle is a guess
        // about which region of the binary moved.
        let summary = report.drift[0]
            .differing_bytes_summary
            .as_deref()
            .expect("drift row must populate differing_bytes_summary");
        assert!(
            summary.contains("offset 0x0"),
            "summary should point at byte 0 for diverging single-byte prefixes. got={summary}"
        );

        // Both artifacts appear in `artifacts`, with the stable one
        // marked deterministic and the drifting one marked not.
        let stable = report
            .artifacts
            .iter()
            .find(|a| a.name == "stable.tar.gz")
            .unwrap();
        let drifting = report
            .artifacts
            .iter()
            .find(|a| a.name == "drifting.tar.gz")
            .unwrap();
        assert!(stable.deterministic);
        assert!(!drifting.deterministic);
        assert!(drifting.hash.is_none());
        assert_eq!(drifting.hashes.len(), 2);
    }

    #[test]
    fn harness_excludes_allowlisted_artifacts_from_drift() {
        let mut h = empty_harness();
        h.allowlist.compile_time.push(AllowListEntry {
            artifact: "*.crate".into(),
            reason: "cargo package non-determinism".into(),
        });
        let runs = run_with_files(
            &h,
            vec![
                vec![("anodizer-0.2.1.crate", b"crate-bytes-A")],
                vec![("anodizer-0.2.1.crate", b"crate-bytes-B")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "allowlisted artifact must not bump drift_count"
        );
        // Artifact still shows up with deterministic=false + the reason
        // populated so the audit trail is intact.
        let row = &report.artifacts[0];
        assert_eq!(row.name, "anodizer-0.2.1.crate");
        assert!(!row.deterministic);
        assert_eq!(
            row.nondeterministic_reason.as_deref(),
            Some("cargo package non-determinism")
        );
        assert_eq!(row.hashes.len(), 2);
    }

    #[test]
    fn harness_treats_missing_artifact_in_one_run_as_drift() {
        let h = empty_harness();
        let runs = run_with_files(&h, vec![vec![("only-in-run-1.tar.gz", b"present")], vec![]]);
        let report = h.build_report(runs);
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.drift[0].artifact, "only-in-run-1.tar.gz");
        assert!(report.drift[0].hashes.iter().any(|h| h == "<missing>"));
    }

    #[test]
    fn stage_inference_matches_known_extensions() {
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.zip"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.crate"), "cargo-package");
        assert_eq!(infer_stage_from_path("dist/foo.sbom.json"), "sbom");
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz.sig"), "sign");
        assert_eq!(infer_stage_from_path("dist/checksums.txt"), "checksum");
        assert_eq!(infer_stage_from_path("dist/SHA256SUMS"), "checksum");
        assert_eq!(infer_stage_from_path("dist/mystery.bin"), "unknown");
        // Windows-native separators must still classify correctly.
        assert_eq!(
            infer_stage_from_path(".det-tmp\\target\\x86_64-pc-windows-msvc\\release\\anodize.exe"),
            "build"
        );
        assert_eq!(infer_stage_from_path("dist\\foo.tar.gz"), "archive");
    }

    #[test]
    fn matches_artifact_pattern_handles_glob_and_exact() {
        assert!(matches_artifact_pattern("*.crate", "foo.crate"));
        assert!(!matches_artifact_pattern("*.crate", "foo.tar.gz"));
        assert!(matches_artifact_pattern("exact.bin", "exact.bin"));
        assert!(!matches_artifact_pattern("exact.bin", "other.bin"));
    }

    #[test]
    fn allow_listed_path_inherits_host_path() {
        // The harness inherits the host PATH verbatim on every platform
        // — its hermeticity goal is CARGO_HOME/HOME isolation, not PATH
        // narrowing. Two runs from the same host process see identical
        // PATH, so determinism is preserved while the harness stays
        // cross-platform (macOS Homebrew at `/opt/homebrew/bin`, Linux
        // at `/usr/bin` / `~/.cargo/bin`, Windows Git at
        // `C:\Program Files\Git\cmd`, etc.). The Windows MSVC vs.
        // Git-Bash `link.exe` shadowing concern is handled by the
        // inherit-everything pass in `build_subprocess_env`: cc-rs
        // resolves MSVC's `link.exe` to an absolute path via env +
        // registry, so rustc never relies on PATH ordering.
        // SAFETY: this test is read-only on the env; no need to
        // serialize.
        let expected = std::env::var("PATH").unwrap_or_default();
        assert_eq!(allow_listed_path(), expected);
    }

    #[test]
    fn stage_id_round_trips_to_string() {
        assert_eq!(StageId::Build.as_str(), "build");
        assert_eq!(StageId::Archive.as_str(), "archive");
        assert_eq!(StageId::Sbom.as_str(), "sbom");
        assert_eq!(StageId::Sign.as_str(), "sign");
        assert_eq!(StageId::Checksum.as_str(), "checksum");
    }

    /// Default `--stages=build,archive,sbom,sign,checksum` MUST drive
    /// `compute_extra_skip` to emit produce-stages like `nfpm`, `nsis`,
    /// `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`,
    /// `appbundle`, `srpm`, `upx`, `makeself`, `notarize`. The release
    /// run 25967997789 failure happened because none of these were
    /// being skipped — the child release subprocess attempted `nfpm pkg
    /// --packager deb` on a macOS shard and died with `No such file or
    /// directory`. This test pins the contract that prevented that
    /// failure.
    #[test]
    fn harness_extra_skip_with_default_stages_includes_nfpm() {
        let stages = vec![
            StageId::Build,
            StageId::Archive,
            StageId::Sbom,
            StageId::Sign,
            StageId::Checksum,
        ];
        let extra = compute_extra_skip(&stages);
        for name in [
            "nfpm",
            "nsis",
            "msi",
            "dmg",
            "pkg",
            "snapcraft",
            "source",
            "flatpak",
            "appbundle",
            "srpm",
            "upx",
            "makeself",
            "notarize",
        ] {
            assert!(
                extra.iter().any(|s| s == name),
                "compute_extra_skip(default-stages) missing `{name}`: {extra:?}"
            );
        }
    }

    /// PRESERVE_SET stages MUST never appear in the extra skip list,
    /// regardless of whether the operator listed them via `--stages=`.
    /// Skipping `validate` would let bad configs through; skipping
    /// `before` would silently drop user hooks; skipping `changelog`
    /// would strip release-notes context downstream stages may read.
    #[test]
    fn harness_extra_skip_omits_preserve_set() {
        let stages = vec![StageId::Build, StageId::Archive];
        let extra = compute_extra_skip(&stages);
        for name in PRESERVE_SET {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip emitted PRESERVE_SET stage `{name}`: {extra:?}"
            );
        }
    }

    /// If the operator names a produce-stage in `--stages=`, the harness
    /// MUST NOT add it to the extra skip list — that would defeat the
    /// whole point of asking for it. The dispatcher's `parse_stages`
    /// only accepts the canonical build-side set today, but
    /// `compute_extra_skip` is written generically against `StageId`'s
    /// `as_str()` so a future extension (e.g. adding `nfpm` as a
    /// `StageId` variant) still gets the right behavior.
    #[test]
    fn harness_extra_skip_omits_requested_stages() {
        let stages = vec![StageId::Build, StageId::Archive, StageId::Sign];
        let extra = compute_extra_skip(&stages);
        for name in ["build", "archive", "sign"] {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip dropped requested stage `{name}`: {extra:?}"
            );
        }
    }

    /// `SIDE_EFFECT_STAGES` entries are added back unconditionally by
    /// the runner's `compute_skip_arg`, so the harness's complement set
    /// shouldn't double-list them. (Pure efficiency / readability
    /// guarantee — `compute_skip_arg` already de-dupes for safety.)
    #[test]
    fn harness_extra_skip_excludes_side_effect_stages() {
        use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
        let stages = vec![StageId::Build];
        let extra = compute_extra_skip(&stages);
        for &name in SIDE_EFFECT_STAGES {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip double-listed side-effect stage `{name}`: {extra:?}"
            );
        }
    }

    #[test]
    fn report_drift_count_matches_drift_array_len() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("a.tar.gz", b"x"), ("b.tar.gz", b"y"), ("c.tar.gz", b"z")],
                vec![
                    ("a.tar.gz", b"x"),
                    ("b.tar.gz", b"y-different"),
                    ("c.tar.gz", b"z-different"),
                ],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.drift.len() as u32, report.drift_count);
        assert_eq!(report.drift_count, 2);
    }

    #[test]
    fn pick_first_artifact_for_stage_picks_first_by_inferred_stage() {
        let artifacts = vec![
            PathBuf::from("dist/checksums.txt"),
            PathBuf::from("dist/foo.tar.gz"),
            PathBuf::from("dist/bar.tar.gz"),
        ];
        let pick = pick_first_artifact_for_stage(&artifacts, "archive").unwrap();
        assert_eq!(pick, &PathBuf::from("dist/foo.tar.gz"));
        let pick = pick_first_artifact_for_stage(&artifacts, "checksum").unwrap();
        assert_eq!(pick, &PathBuf::from("dist/checksums.txt"));
    }

    #[test]
    fn pick_first_artifact_for_stage_returns_none_for_missing_stage() {
        let artifacts = vec![PathBuf::from("dist/foo.tar.gz")];
        assert!(pick_first_artifact_for_stage(&artifacts, "sbom").is_none());
        assert!(pick_first_artifact_for_stage(&artifacts, "bogus-stage").is_none());
    }

    #[test]
    fn inject_drift_byte_mutates_file_so_hash_differs() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("victim.bin");
        std::fs::write(&p, b"hello world").unwrap();
        let before = {
            let mut h = Sha256::new();
            h.update(std::fs::read(&p).unwrap());
            format!("{:x}", h.finalize())
        };
        inject_drift_byte(&p).expect("inject");
        let after_bytes = std::fs::read(&p).unwrap();
        let after = {
            let mut h = Sha256::new();
            h.update(&after_bytes);
            format!("{:x}", h.finalize())
        };
        assert_ne!(before, after, "hash must change after drift injection");
        assert_eq!(
            after_bytes.len(),
            b"hello world".len() + 1,
            "exactly one byte must be appended"
        );
    }

    /// `discover_artifacts` MUST surface raw cargo binaries from
    /// `<worktree>/.det-tmp/target/<triple>/release/<bin>` AND
    /// `<worktree>/.det-tmp/target/release/<bin>`, alongside `dist/`
    /// artifacts, with the raw binaries getting a `target/...` map key
    /// prefix so the report distinguishes them from any same-basename
    /// `dist/` files. Closes the diagnostic gap where binary-level
    /// RUSTFLAGS / mtime drift was only observable through six layers
    /// of wrapper archives.
    #[test]
    fn discover_artifacts_includes_raw_cargo_binaries() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();

        // dist artifact (existing surface)
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("anodize_0.3.0_linux_amd64.tar.gz"), b"archive").unwrap();

        // Cross-target build outputs
        let triple_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-unknown-linux-gnu")
            .join("release");
        std::fs::create_dir_all(&triple_release).unwrap();
        std::fs::write(triple_release.join("anodize"), b"raw-bin-linux").unwrap();
        // depfile must NOT be surfaced (cargo scratch).
        std::fs::write(triple_release.join("anodize.d"), b"depfile").unwrap();
        // `deps/` subdirectory must NOT be recursed (cargo scratch).
        std::fs::create_dir_all(triple_release.join("deps")).unwrap();
        std::fs::write(triple_release.join("deps").join("libfoo.rlib"), b"rlib").unwrap();

        // Windows-style triple with .exe
        let win_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-pc-windows-msvc")
            .join("release");
        std::fs::create_dir_all(&win_release).unwrap();
        std::fs::write(win_release.join("anodize.exe"), b"raw-bin-windows").unwrap();
        // .pdb debug symbols must NOT be surfaced.
        std::fs::write(win_release.join("anodize.pdb"), b"pdb").unwrap();

        // Host build (no triple): target/release/anodize.
        let host_release = wt.join(".det-tmp").join("target").join("release");
        std::fs::create_dir_all(&host_release).unwrap();
        std::fs::write(host_release.join("anodize"), b"raw-bin-host").unwrap();

        let artifacts = discover_artifacts(wt).expect("discover");
        let names: Vec<String> = artifacts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            names
                .iter()
                .any(|n| n == "anodize_0.3.0_linux_amd64.tar.gz"),
            "dist artifact missing: {names:?}"
        );
        // Three raw binaries: linux triple, windows triple, host release.
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "anodize").count(),
            2,
            "expected 2 `anodize` raw binaries (linux + host), got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "anodize.exe"),
            "windows raw binary missing: {names:?}"
        );

        // Scratch files must NOT be surfaced.
        for forbidden in ["anodize.d", "anodize.pdb", "libfoo.rlib"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "cargo scratch `{forbidden}` leaked into discovery: {names:?}"
            );
        }

        // hash_artifacts must label the raw binaries with a `target/...`
        // map key so the report distinguishes them from `dist/`.
        let map = hash_artifacts(wt, &artifacts).expect("hash");
        let target_keys: Vec<&String> = map.keys().filter(|k| k.starts_with("target/")).collect();
        assert_eq!(
            target_keys.len(),
            3,
            "expected 3 `target/...`-prefixed map keys, got: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        // Forward slashes regardless of host platform.
        for k in &target_keys {
            assert!(
                !k.contains('\\'),
                "raw-binary map key contains backslash: {k}"
            );
        }
        // Spot-check one key shape.
        assert!(
            target_keys
                .iter()
                .any(|k| { k.as_str() == "target/x86_64-unknown-linux-gnu/release/anodize" }),
            "expected `target/x86_64-unknown-linux-gnu/release/anodize` key, got: {target_keys:?}"
        );
        // Raw binaries get `build` stage attribution so the diagnostic
        // chain reads build → archive → checksum → sign.
        for k in &target_keys {
            assert_eq!(
                map.get(k.as_str()).map(|i| i.stage.as_str()),
                Some("build"),
                "raw binary `{k}` must be attributed to `build` stage"
            );
        }
    }

    /// `discover_artifacts` must tolerate a missing `.det-tmp/target`
    /// (e.g. the harness has only just spawned and the child hasn't
    /// produced anything yet) — it shouldn't error out.
    #[test]
    fn discover_artifacts_tolerates_missing_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        // Just dist/, no .det-tmp/.
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("foo.tar.gz"), b"x").unwrap();
        let out = discover_artifacts(wt).expect("must not error on missing target dir");
        assert_eq!(out.len(), 1);
    }

    // ── I7 — build_subprocess_env env allow-list ─────────────────────────
    //
    // Env-var reads happen on the live process env, so these tests
    // serialize on the `harness_env` group. SAFETY: `std::env::set_var` /
    // `remove_var` are unsafe in multi-threaded processes from Rust 2024
    // onward; the `serial_test::serial(harness_env)` annotation ensures
    // exclusive ownership of the env for the duration of each test.
    //
    // Pattern: `unsafe { std::env::remove_var(k) }` before AND after each
    // env-touching test so a leaked value from a previous run can't poison
    // an assertion (`harness_env_omits_unset_github_vars` in particular).

    mod env_allowlist {
        use super::*;
        use serial_test::serial;

        fn inputs<'a>(scratch: &'a Path) -> BuildSubprocessEnv<'a> {
            BuildSubprocessEnv {
                cargo_home: scratch,
                cargo_target: scratch,
                tmpdir: scratch,
                home_dir: scratch,
                sde: 1_715_000_000,
                worktree: scratch,
                signing_keys: None,
            }
        }

        fn with_cleared<F: FnOnce()>(keys: &[&str], f: F) {
            // SAFETY: gated by `#[serial(harness_env)]` on every caller.
            for k in keys {
                unsafe { std::env::remove_var(k) };
            }
            f();
            for k in keys {
                unsafe { std::env::remove_var(k) };
            }
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_does_not_leak_github_token() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["GITHUB_TOKEN"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("GITHUB_TOKEN", "ghp_secret_value") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("GITHUB_TOKEN"),
                    "GITHUB_TOKEN must NOT propagate into the harness subprocess env (regression: I7)"
                );
                // Also: no value of the env map should equal the token —
                // belt-and-braces against an accidental rename / aliasing.
                assert!(
                    !env.values().any(|v| v == "ghp_secret_value"),
                    "no env entry may carry the token value"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_does_not_leak_actions_runtime_token() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["ACTIONS_RUNTIME_TOKEN"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("ACTIONS_RUNTIME_TOKEN", "actions_secret") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
                    "ACTIONS_RUNTIME_TOKEN must NOT propagate into the harness subprocess env"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_does_not_leak_actions_cache_url() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["ACTIONS_CACHE_URL"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("ACTIONS_CACHE_URL", "https://cache.example") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("ACTIONS_CACHE_URL"),
                    "ACTIONS_CACHE_URL must NOT propagate (network-reach surface)"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_includes_github_repository_when_set() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["GITHUB_REPOSITORY"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("GITHUB_REPOSITORY", "toss45/anodizer") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("GITHUB_REPOSITORY").map(String::as_str),
                    Some("toss45/anodizer"),
                    "GITHUB_REPOSITORY is identity and must propagate"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_includes_github_sha_when_set() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["GITHUB_SHA"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("GITHUB_SHA", "deadbeefcafe") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("GITHUB_SHA").map(String::as_str),
                    Some("deadbeefcafe"),
                    "GITHUB_SHA is identity and must propagate"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_includes_runner_identity_vars_when_set() {
            // Symmetry guard for the GITHUB_REPOSITORY / GITHUB_SHA
            // positive-inheritance tests above. The allow-list line is
            // the contract, but iterating each RUNNER_* identity var
            // catches drift if a future edit drops one entry.
            let tmp = tempfile::tempdir().unwrap();
            let cases = [
                ("RUNNER_OS", "Linux"),
                ("RUNNER_ARCH", "X64"),
                ("RUNNER_NAME", "self-hosted-1"),
            ];
            with_cleared(&cases.map(|(k, _)| k), || {
                for (k, v) in cases {
                    // SAFETY: serial(harness_env) holds the lock.
                    unsafe { std::env::set_var(k, v) };
                }
                let env = build_subprocess_env(&inputs(tmp.path()));
                for (k, v) in cases {
                    assert_eq!(
                        env.get(k).map(String::as_str),
                        Some(v),
                        "{k} is identity and must propagate (value `{v}`)"
                    );
                }
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_omits_unset_github_vars() {
            let tmp = tempfile::tempdir().unwrap();
            // Pre-clear every identity var so the host process's own env
            // (a real GHA runner) can't inject a value.
            let all_identity = [
                "GITHUB_REPOSITORY",
                "GITHUB_SHA",
                "GITHUB_REF",
                "GITHUB_REF_NAME",
                "GITHUB_RUN_ID",
                "GITHUB_RUN_NUMBER",
                "GITHUB_WORKFLOW",
                "GITHUB_ACTOR",
            ];
            with_cleared(&all_identity, || {
                let env = build_subprocess_env(&inputs(tmp.path()));
                for k in all_identity {
                    assert!(
                        !env.contains_key(k),
                        "unset host var `{k}` must not appear in env (no empty-string default)"
                    );
                }
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_does_not_leak_runner_temp() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUNNER_TEMP"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("RUNNER_TEMP", "/some/host/tmpdir") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                // RUNNER_TEMP would point outside the worktree; the
                // harness pins TMPDIR to a per-run worktree subdir.
                assert!(
                    !env.contains_key("RUNNER_TEMP"),
                    "RUNNER_TEMP must NOT propagate — harness owns TMPDIR"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_sets_ci_true_when_host_lacks_it() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["CI"], || {
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("CI").map(String::as_str),
                    Some("true"),
                    "harness defaults CI=true when host has no CI var set"
                );
            });
        }

        // ── RUSTUP_HOME defaulting (W5) ─────────────────────────────────
        //
        // rustup needs RUSTUP_HOME to dispatch a toolchain. GH Actions
        // runners and most dev machines don't set it; rustup falls back to
        // `$HOME/.rustup`, but the harness's per-run HOME is a fresh
        // tmpdir, so the fallback dereferences to an empty rustup root.
        // The harness must synthesize a default from the HOST's HOME
        // (Unix) or USERPROFILE (Windows) so a cleared child can still
        // find a toolchain.

        /// Restore the host's HOME on Drop so RUSTUP_HOME tests can mutate
        /// it under the serial(harness_env) lock without leaking a fake
        /// value into sibling tests that read HOME (e.g. the next test in
        /// the env_allowlist module).
        struct HomeGuard {
            previous: Option<std::ffi::OsString>,
        }
        impl HomeGuard {
            fn capture() -> Self {
                Self {
                    previous: std::env::var_os("HOME"),
                }
            }
        }
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                // SAFETY: caller holds serial(harness_env).
                match &self.previous {
                    Some(v) => unsafe { std::env::set_var("HOME", v) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_defaults_rustup_home_from_host_home_when_unset() {
            let tmp = tempfile::tempdir().unwrap();
            let _home = HomeGuard::capture();
            with_cleared(&["RUSTUP_HOME"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("HOME", "/host/home/user") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                let rh = env
                    .get("RUSTUP_HOME")
                    .expect("RUSTUP_HOME must be defaulted when unset")
                    .replace('\\', "/");
                assert_eq!(
                    rh, "/host/home/user/.rustup",
                    "harness must default RUSTUP_HOME to <host HOME>/.rustup"
                );
            });
        }

        #[test]
        #[serial(harness_env)]
        fn harness_env_rustup_home_explicit_wins_over_default() {
            let tmp = tempfile::tempdir().unwrap();
            let _home = HomeGuard::capture();
            with_cleared(&["RUSTUP_HOME"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("HOME", "/host/home/user") };
                unsafe { std::env::set_var("RUSTUP_HOME", "/operator/override") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("RUSTUP_HOME").map(String::as_str),
                    Some("/operator/override"),
                    "an explicit host RUSTUP_HOME must take precedence over the synthesized default"
                );
            });
        }

        // ── Windows inherit-everything deny-list ─────────────────────────
        //
        // The Windows pass inherits the FULL host env minus
        // [`WINDOWS_ENV_DENYLIST`] + the suffix sweep. Allow-list pass
        // results above are platform-agnostic; these tests pin the
        // Windows-only deny-list behavior so a future allow-list edit
        // can't accidentally re-leak credentials.

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_inherits_host_system_vars() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["PROGRAMFILES"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("PROGRAMFILES", r"C:\fake\Program Files") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("PROGRAMFILES").map(String::as_str),
                    Some(r"C:\fake\Program Files"),
                    "Windows pass must inherit non-credential host system vars (PROGRAMFILES is load-bearing for cc-rs link.exe discovery)"
                );
            });
        }

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_drops_credentials() {
            let tmp = tempfile::tempdir().unwrap();
            let keys = [
                "GITHUB_TOKEN",
                "CARGO_REGISTRY_TOKEN",
                "SOMETHING_TOKEN",
                "SOMETHING_PASSWORD",
            ];
            with_cleared(&keys, || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe {
                    std::env::set_var("GITHUB_TOKEN", "ghp_x");
                    std::env::set_var("CARGO_REGISTRY_TOKEN", "cratesio_y");
                    std::env::set_var("SOMETHING_TOKEN", "z");
                    std::env::set_var("SOMETHING_PASSWORD", "w");
                }
                let env = build_subprocess_env(&inputs(tmp.path()));
                for k in keys {
                    assert!(
                        !env.contains_key(k),
                        "credential-bearing host var `{k}` must NOT propagate on Windows (deny-list + suffix sweep)"
                    );
                }
                // Belt-and-braces: none of the *values* may leak either.
                for v in ["ghp_x", "cratesio_y", "z", "w"] {
                    assert!(
                        !env.values().any(|got| got == v),
                        "credential value `{v}` leaked under a different key"
                    );
                }
            });
        }

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_drops_actions_workflow_internals() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["ACTIONS_RUNTIME_TOKEN"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("ACTIONS_RUNTIME_TOKEN", "actions_x") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("ACTIONS_RUNTIME_TOKEN"),
                    "ACTIONS_* workflow-internal vars must be dropped by the Windows pass"
                );
            });
        }

        // ── Windows hermeticity sweep: GH/RUNNER namespace ───────────────
        //
        // `GITHUB_*` / `RUNNER_*` vars NOT on `HARNESS_ENV_ALLOWLIST` are
        // host workflow state (path-pointing or runner-side scratch). The
        // Linux/macOS path drops them naturally (strip-everything-except-
        // allow-list); the Windows inherit-everything pass must also drop
        // them via `windows_env_should_drop`'s namespace gate or
        // hermeticity breaks (cargo / cc-rs / Win32 would see runner-owned
        // paths instead of the per-run worktree).
        //
        // Regression guard: the inverse-by-deny-list redesign at d1bd9eb
        // originally missed this carve-out; this set of tests pins it.

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_drops_runner_temp_for_hermeticity() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUNNER_TEMP"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("RUNNER_TEMP", r"C:\fake\temp") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("RUNNER_TEMP"),
                    "RUNNER_TEMP must NOT propagate on Windows — it points at the runner's on-host scratch and the harness owns TMPDIR"
                );
            });
        }

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_drops_runner_workspace_for_hermeticity() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUNNER_WORKSPACE"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("RUNNER_WORKSPACE", r"C:\fake\workspace") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("RUNNER_WORKSPACE"),
                    "RUNNER_WORKSPACE must NOT propagate on Windows — host workflow state, not identity"
                );
            });
        }

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_drops_github_workspace_for_hermeticity() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["GITHUB_WORKSPACE"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("GITHUB_WORKSPACE", r"C:\fake\gh_workspace") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert!(
                    !env.contains_key("GITHUB_WORKSPACE"),
                    "GITHUB_WORKSPACE must NOT propagate on Windows — points at the GH-runner-owned checkout, not the hermetic worktree"
                );
            });
        }

        /// Regression: the harness MUST inject
        /// `--remap-path-prefix=<worktree>=/anodize` so two from-clean
        /// runs at different worktree paths produce a byte-identical
        /// anodizer binary. Without this, rustc embeds the absolute
        /// worktree path into every `file!()` / `Location::caller()`
        /// expansion, drifting the binary and every archive that wraps
        /// it. CI run 25975073213 surfaced this drift on every platform
        /// shard before this knob landed.
        #[test]
        #[serial(harness_env)]
        fn harness_env_injects_remap_path_prefix_for_worktree() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUSTFLAGS"], || {
                let env = build_subprocess_env(&inputs(tmp.path()));
                let rf = env.get("RUSTFLAGS").expect(
                    "RUSTFLAGS must be injected so worktree paths don't leak into the binary",
                );
                let needle = format!(
                    "--remap-path-prefix={}=/anodize",
                    tmp.path().to_string_lossy()
                );
                assert!(
                    rf.contains(&needle),
                    "RUSTFLAGS must remap the worktree path. got={rf}, expected substring={needle}"
                );
                // The cargo_home / cargo_target prefixes (which `inputs`
                // points at the same scratch dir) are also remapped.
                assert!(
                    rf.contains("=/cargo"),
                    "CARGO_HOME must be remapped to /cargo (defense-in-depth against std/proc-macro path leakage)"
                );
                assert!(
                    rf.contains("=/target"),
                    "CARGO_TARGET_DIR must be remapped to /target"
                );
            });
        }

        /// Pre-existing RUSTFLAGS (e.g. operator-supplied `-C linker=...`
        /// for cross-compile) MUST be preserved — we append, not
        /// overwrite. Otherwise a Windows MSVC link-flag would silently
        /// disappear under the harness path.
        #[test]
        #[serial(harness_env)]
        fn harness_env_preserves_host_rustflags() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUSTFLAGS"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("RUSTFLAGS", "-C linker=link.exe -C link-arg=/DEBUG") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                let rf = env.get("RUSTFLAGS").unwrap();
                assert!(
                    rf.contains("-C linker=link.exe"),
                    "host RUSTFLAGS must survive the harness append. got={rf}"
                );
                assert!(
                    rf.contains("--remap-path-prefix="),
                    "remap-path-prefix must be appended even when host RUSTFLAGS is set. got={rf}"
                );
            });
        }

        /// Regression: the harness MUST inject
        /// `CARGO_TARGET_<msvc-triple>_RUSTFLAGS=-C link-arg=/Brepro`
        /// so two harness runs produce byte-identical `anodizer.exe`
        /// binaries. Without `/Brepro`, link.exe stamps the PE COFF
        /// `TimeDateStamp` with wall-clock time and the .exe (plus
        /// every archive wrapping it) drifts. CI shard
        /// "Determinism (windows-latest)" surfaced 20 drifted
        /// artifacts before this knob landed in cycle 12.
        ///
        /// Per-target (not global) because `/Brepro` is link.exe-only;
        /// lld/ld would reject the flag.
        ///
        /// Per-target RUSTFLAGS must ALSO carry the remap-path-prefix
        /// entries: cargo precedence is `CARGO_TARGET_<triple>_RUSTFLAGS`
        /// > `RUSTFLAGS`, so the per-target value REPLACES (not merges
        /// with) the global. Dropping the remap on MSVC would
        /// re-introduce the worktree-path drift the cycle-8 fix
        /// closed.
        #[test]
        #[serial(harness_env)]
        fn harness_env_injects_msvc_determinism_flags() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUSTFLAGS"], || {
                let env = build_subprocess_env(&inputs(tmp.path()));
                for triple_env in [
                    "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                    "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS",
                ] {
                    let rf = env.get(triple_env).unwrap_or_else(|| {
                        panic!("{triple_env} must be injected so link.exe gets /Brepro")
                    });
                    for needle in [
                        "-C link-arg=/Brepro",
                        "-C link-arg=/PDBALTPATH:%_PDB%",
                        "-C link-arg=/DEBUG:NONE",
                    ] {
                        assert!(
                            rf.contains(needle),
                            "{triple_env} must carry `{needle}`. got={rf}"
                        );
                    }
                    assert!(
                        rf.contains("--remap-path-prefix="),
                        "{triple_env} must also carry --remap-path-prefix (per-target rustflags REPLACES global RUSTFLAGS, not appends). got={rf}"
                    );
                }
                // Linux / macOS targets must NOT get a per-target
                // entry — `/Brepro` would error on lld/ld.
                for triple_env in [
                    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
                    "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS",
                ] {
                    assert!(
                        !env.contains_key(triple_env),
                        "{triple_env} must NOT be injected — /Brepro is link.exe-only"
                    );
                }
            });
        }

        /// Regression: on Windows, global RUSTFLAGS must ALSO carry the
        /// MSVC determinism flags so the host build (e.g.
        /// `cargo run --release` invoked by a `before:` hook, which has
        /// no `--target` and therefore reads global RUSTFLAGS) lands a
        /// byte-stable `target/release/anodizer.exe`. Without this, the
        /// host build drifts at PE offset 0x108 (TimeDateStamp) even
        /// though the per-target builds at
        /// `target/<msvc-triple>/release/` are reproducible.
        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_injects_msvc_flags_into_global_rustflags() {
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUSTFLAGS"], || {
                let env = build_subprocess_env(&inputs(tmp.path()));
                let rf = env.get("RUSTFLAGS").expect(
                    "RUSTFLAGS must be set on Windows so host builds (no --target) are reproducible"
                );
                for needle in [
                    "-C link-arg=/Brepro",
                    "-C link-arg=/PDBALTPATH:%_PDB%",
                    "-C link-arg=/DEBUG:NONE",
                ] {
                    assert!(
                        rf.contains(needle),
                        "global RUSTFLAGS must carry `{needle}` on Windows. got={rf}"
                    );
                }
                assert!(
                    rf.contains("--remap-path-prefix="),
                    "global RUSTFLAGS must also carry --remap-path-prefix. got={rf}"
                );
            });
        }

        #[test]
        #[cfg(windows)]
        #[serial(harness_env)]
        fn harness_env_windows_keeps_runner_os_allow_listed() {
            // Positive control for the hermeticity gate: identity vars on
            // `HARNESS_ENV_ALLOWLIST` must still propagate. Catches a
            // future skip-predicate regression that over-broadens the
            // namespace drop.
            let tmp = tempfile::tempdir().unwrap();
            with_cleared(&["RUNNER_OS"], || {
                // SAFETY: serial(harness_env) holds the lock.
                unsafe { std::env::set_var("RUNNER_OS", "Windows") };
                let env = build_subprocess_env(&inputs(tmp.path()));
                assert_eq!(
                    env.get("RUNNER_OS").map(String::as_str),
                    Some("Windows"),
                    "RUNNER_OS is on the identity allow-list and MUST propagate even though the namespace gate would otherwise drop it"
                );
            });
        }
    }
}
