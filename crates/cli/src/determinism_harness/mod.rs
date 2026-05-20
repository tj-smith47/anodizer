//! Determinism harness — drives N from-clean rebuilds in hermetic
//! worktrees and diffs the emitted artifacts.
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
//!    from the host; plus an identity-only allow-list — see [`env`].
//! 3. Invokes the build-side pipeline (`anodize release --snapshot
//!    --skip=<SIDE_EFFECT_STAGES>`) inside the worktree with that env.
//! 4. Walks `<worktree>/dist` AND `<worktree>/.det-tmp/target/`,
//!    SHA256s every file, returns a `BTreeMap<artifact_name, info>`
//!    for the run.
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
//! actually want to assert.
//!
//! ## Allow-list semantics
//!
//! Allow-list matching uses the same `*.ext` glob semantics as
//! [`anodizer_core::DeterminismState::resolve_reason`]: a leading `*` is
//! a suffix-match, anything else is exact-match. Compile-time matches
//! win on collision; the matched reason populates
//! [`ArtifactRow::nondeterministic_reason`].

mod artifacts;
mod drift;
mod env;
mod preserve;

use anodizer_core::git::worktree::Worktree;
use anodizer_core::harness_signing::EphemeralSigningKeys;
use anodizer_core::{AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use artifacts::{
    ArtifactInfo, copy_artifacts_to_dump, discover_artifacts, hash_artifacts, prune_dump_to_drifted,
};
use drift::{inject_drift_byte, pick_first_artifact_for_stage, summarize_drift};
use env::{BuildSubprocessEnv, build_subprocess_env};
use preserve::{
    ContextInputs, preserve_dist_tree, remove_preserved_on_drift, write_preserved_dist_context,
};

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
    Makeself,
    Snapcraft,
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
            StageId::Makeself => "makeself",
            StageId::Snapcraft => "snapcraft",
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
/// - `templatefiles` — pre-build template materialization.
///
/// Adding any of these to the child `--skip=` list would break stages
/// that depend on their side-effects-on-context (not on disk), which is
/// why the harness's complement-set calculation subtracts them.
const PRESERVE_SET: &[&str] = &["validate", "before", "templatefiles"];

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
/// Why this matters: `--stages=` is the harness's "what to diff" filter,
/// but it does NOT restrict which stages the child release subprocess
/// runs. Without this complement set the child runs the full pipeline
/// (minus side-effects), including produce-stages like `nfpm`, `nsis`,
/// `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`, `appbundle`,
/// `srpm`, `upx`, `makeself`, `notarize`. On macOS / Windows shards
/// those binaries aren't installed; on Linux shards some are but the
/// target artifacts don't exist on a non-native shard.
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
    /// Destination path for the JSON report. The CLI dispatcher owns
    /// writing the file; the harness uses the parent dir as the root
    /// for the drift-bins dump.
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
    /// configured target.
    pub targets: Option<Vec<String>>,
    /// `--preserve-dist=<path>`: when set AND `drift_count == 0`, copy
    /// `<worktree>/dist/**` from the first run to this path before the
    /// worktree is destroyed, then emit a `context.json` manifest
    /// describing the preserved artifact set. Consumed by the release
    /// workflow's publish-only flow so the determinism step's output
    /// can be shipped directly without a redundant rebuild.
    ///
    /// The copy happens at the end of run 0 (run-0 and run-N are
    /// byte-identical by construction once the harness passes; run-0 is
    /// picked deterministically). If the harness later detects drift
    /// across runs, the preserved directory is removed so shippable
    /// bytes never escape a failed determinism check.
    pub preserve_dist: Option<PathBuf>,
    /// Fallback version string used in `context.json` when the
    /// preserved-dist's `metadata.json` is missing or malformed.
    /// Dispatcher resolves this from the snapshot template variables
    /// (or `Cargo.toml` for non-snapshot runs) so the manifest's
    /// `version` field is non-empty even when the sibling JSON
    /// vanishes. Pass an empty string to keep the prior behaviour
    /// (manifest `version` empty when JSON missing).
    ///
    /// Unused when `preserve_dist` is `None`.
    pub version_hint: String,
    /// Whether to pass `--snapshot` to the child `anodize release ...`
    /// subprocess. `true` (the default / legacy behaviour) emits
    /// artifacts named with the snapshot version suffix
    /// (`-SNAPSHOT-<sha>`); `false` drops the flag so produce-stages
    /// emit artifacts named with the actual release version. The
    /// release workflow flips this off on tag-push runs so the bytes
    /// preserved by `--preserve-dist` are immediately shippable via
    /// `anodize release --publish-only`.
    pub child_snapshot: bool,
}

impl Harness {
    /// Drive the harness end-to-end and return the populated report.
    ///
    /// Does NOT write the report — the CLI dispatcher is responsible for
    /// serializing the returned `DeterminismReport` and exiting non-zero
    /// when `drift_count > 0`.
    pub fn run(&self) -> Result<DeterminismReport> {
        let mut per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>> =
            Vec::with_capacity(self.runs as usize);

        // Preserve-dist + production-keys → skip Sign in the harness.
        //
        // When the workflow plans to ship the harness's output via the
        // publish-only path (`--preserve-dist=<path>` set on the harness;
        // `COSIGN_KEY` / `GPG_PRIVATE_KEY` exported on the runner), the
        // harness's ephemeral signatures would land in the preserved dist
        // and have to be stripped before re-signing with production keys.
        // Cleaner to never write them: skip the Sign stage entirely.
        //
        // KNOWN COVERAGE GAP: byte-stability of the Sign stage is no
        // longer exercised in CI when this branch fires. Acceptable
        // tradeoff — the `harness_signing` unit tests already pin the
        // SDE-based key derivation (cosign-keygen + GPG `--faked-system-
        // time`) so the deterministic-keys property has direct coverage,
        // and the production sign stage is exercised by every release.
        let skip_sign_for_preserve = self.preserve_dist.is_some()
            && (std::env::var_os("COSIGN_KEY").is_some()
                || std::env::var_os("GPG_PRIVATE_KEY").is_some());
        let effective_stages: Vec<StageId> = if skip_sign_for_preserve {
            self.stages
                .iter()
                .copied()
                .filter(|s| *s != StageId::Sign)
                .collect()
        } else {
            self.stages.clone()
        };

        // Provision once: both runs must sign with identical key
        // material, otherwise even byte-deterministic GPG signatures
        // would diverge. Skipped when `skip_sign_for_preserve` is set
        // (no Sign stage → no keys needed).
        let signing_keys: Option<EphemeralSigningKeys> =
            if effective_stages.contains(&StageId::Sign) {
                Some(anodizer_core::harness_signing::provision_ephemeral_keys(
                    self.sde,
                )?)
            } else {
                None
            };

        // Default to <repo_root>/.det-worktrees/ — keeps the harness
        // off `/tmp` (which is tmpfs on many distros and exhausts fast
        // when the cargo target dir lives inside the worktree). CI
        // (GitHub Actions) sets RUNNER_TEMP to a disk-backed path
        // outside the repo, so honor that when present.
        let worktree_root = std::env::var_os("RUNNER_TEMP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.repo_root.join(".det-worktrees"));
        let _ = std::fs::create_dir_all(&worktree_root);
        // PID-suffix the worktree so parallel harness invocations
        // (cargo test running multiple determinism integration tests
        // concurrently) don't collide on the same path. WITHIN one
        // invocation every run reuses the same path — that's the
        // load-bearing invariant for /Brepro and UTF-16 cargo-registry
        // paths embedded into binaries (drift otherwise cascades from
        // a 2-byte path diff). Across invocations the path must be
        // unique because git worktree add refuses a populated target.
        let worktree_path =
            worktree_root.join(format!("anodize-determinism-{}", std::process::id()));

        for run_idx in 0..self.runs {
            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            let env = self.build_isolated_env(&worktree, signing_keys.as_ref())?;
            self.run_build_pipeline(worktree.path(), &env, &effective_stages)
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
                                format!(
                                    "  {} -> {}",
                                    p.display(),
                                    artifacts::infer_stage_from_path(&s)
                                )
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
            // Preserve run-0's dist tree to the operator-supplied path
            // BEFORE the next iteration's `remove_dir_all` (or this
            // iteration's `Worktree::drop`) wipes it. run-0 is the
            // earliest deterministic pick — runs 1..N are byte-identical
            // to run-0 once the harness passes, but the next run's
            // `remove_dir_all` at the top of the loop deletes the
            // worktree wholesale, so we copy from run-0 specifically.
            //
            // The drift gate happens POST-loop: if drift is detected
            // after all runs finish, we delete the preserved dir below
            // so shippable bytes never escape a failed determinism run.
            if run_idx == 0
                && let Some(dest) = self.preserve_dist.as_ref()
            {
                preserve_dist_tree(worktree.path(), dest).with_context(|| {
                    format!(
                        "preserving run-0 dist tree from {} to {}",
                        worktree.path().join("dist").display(),
                        dest.display()
                    )
                })?;
                // No `preserved_dist_filled` flag needed: any error in
                // preserve_dist_tree propagates via `?` and aborts the
                // harness before the post-loop block runs. Reaching
                // post-loop with `self.preserve_dist == Some(_)` is
                // sufficient proof the copy succeeded.
            }
            // Worktree dropped at end of scope → cleanup automatic.
        }

        let report = self.build_report(per_run_hashes);
        if let Some(parent) = self.report_path.parent() {
            prune_dump_to_drifted(&parent.join("drift-bins"), &report);
        }
        // Preserve-dist gate. Restructured per code review: if any
        // copy failed mid-loop the `?` propagation already aborted the
        // harness, so reaching this point with
        // `self.preserve_dist == Some(_)` means run-0's tree IS on
        // disk under `dest`. Branch on drift_count alone.
        //
        // Safety property: shippable bytes must come from a green
        // determinism run, never a drifted one. Drift → remove the
        // tree; green → write `<dest>/context.json` so the publish-
        // only path can rehydrate.
        if let Some(dest) = self.preserve_dist.as_ref() {
            if report.drift_count > 0 {
                remove_preserved_on_drift(dest);
            } else {
                write_preserved_dist_context(
                    dest,
                    ContextInputs {
                        report: &report,
                        harness_targets: self.targets.as_deref(),
                        version_hint: &self.version_hint,
                    },
                )
                .with_context(|| {
                    format!(
                        "writing context.json under preserved dist {}",
                        dest.display()
                    )
                })?;
            }
        }
        Ok(report)
    }

    /// Construct the env map handed to each child build process.
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
    /// is on the forbid-list for direct subprocess spawn, so the actual
    /// `Command::new` lives in core where it's allow-listed.
    ///
    /// `effective_stages` is what the harness actually ran the child
    /// pipeline against — usually equal to `self.stages`, but with
    /// `Sign` filtered out when [`Harness::preserve_dist`] is set AND
    /// production signing keys are present on the runner (so the harness
    /// doesn't leave ephemeral sigs in the preserved dist; they would
    /// only get stripped + re-signed later anyway).
    fn run_build_pipeline(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        effective_stages: &[StageId],
    ) -> Result<()> {
        let exe = anodizer_core::determinism_runner::current_anodize_binary()?;
        let extra_skip = compute_extra_skip(effective_stages);
        anodizer_core::determinism_runner::run_build_pipeline_subprocess(
            &exe,
            worktree_path,
            env,
            self.targets.as_deref(),
            &extra_skip,
            self.child_snapshot,
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
            // of truth for path/size (matches "last writer wins"
            // semantics for the cosmetic fields).
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
                // NOT bump `drift_count`.
                if allow_reason.is_none() {
                    let summary = summarize_drift(name, &per_run_hashes);
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: summary,
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
    /// entries win on collision.
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

#[cfg(test)]
mod tests {
    use super::artifacts::{HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES, infer_stage_from_path};
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
            preserve_dist: None,
            version_hint: String::new(),
            child_snapshot: true,
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
        // so future fix-cycles aren't blind.
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
    fn matches_artifact_pattern_handles_glob_and_exact() {
        assert!(matches_artifact_pattern("*.crate", "foo.crate"));
        assert!(!matches_artifact_pattern("*.crate", "foo.tar.gz"));
        assert!(matches_artifact_pattern("exact.bin", "exact.bin"));
        assert!(!matches_artifact_pattern("exact.bin", "other.bin"));
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
    /// `appbundle`, `srpm`, `upx`, `makeself`, `notarize`. Without this,
    /// the child release subprocess attempts e.g. `nfpm pkg --packager
    /// deb` on a macOS shard and dies with `No such file or directory`.
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
    /// `before` would silently drop user hooks; skipping `templatefiles`
    /// would leave downstream stages without their materialized inputs.
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

    /// `changelog` is NOT in PRESERVE_SET — its output isn't a built
    /// artifact the harness diffs, `use=github-native` is inherently
    /// non-deterministic (depends on remote API state), and the harness
    /// env strips `GITHUB_TOKEN` for hermeticity so the stage would
    /// bail on tag-push runs. The publish-only path still runs the
    /// changelog stage with the real token, so the GitHub Release body
    /// is unaffected.
    #[test]
    fn harness_extra_skip_includes_changelog() {
        let stages = vec![StageId::Build, StageId::Archive];
        let extra = compute_extra_skip(&stages);
        assert!(
            extra.iter().any(|s| s == "changelog"),
            "compute_extra_skip missing `changelog`: {extra:?}"
        );
    }

    /// If the operator names a produce-stage in `--stages=`, the harness
    /// MUST NOT add it to the extra skip list — that would defeat the
    /// whole point of asking for it.
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
    /// shouldn't double-list them.
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
}
