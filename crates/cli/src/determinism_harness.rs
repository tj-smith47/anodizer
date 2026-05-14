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
//!    `TMPDIR`, `HOME`; `SOURCE_DATE_EPOCH=self.sde`; `PATH` trimmed to
//!    `allow_listed_path()`; everything else stripped except an explicit
//!    identity-only allow-list — see [`HARNESS_ENV_ALLOWLIST`]. Notably
//!    excluded: `GITHUB_TOKEN`, `ACTIONS_RUNTIME_TOKEN`, and every other
//!    credential-bearing var.
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
    Archive,
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
            StageId::Archive => "archive",
            StageId::Sbom => "sbom",
            StageId::Sign => "sign",
            StageId::Checksum => "checksum",
        }
    }
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

        for run_idx in 0..self.runs {
            let worktree_path = std::env::temp_dir().join(format!(
                "anodize-determinism-{}-{}",
                std::process::id(),
                run_idx
            ));
            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            let env = self.build_isolated_env(&worktree)?;
            self.run_build_pipeline(worktree.path(), &env)
                .with_context(|| format!("building pipeline for determinism run {}", run_idx))?;
            let artifacts = discover_artifacts(worktree.path())?;
            // `--inject-drift=<stage>` (test-harness gated): mutate the
            // first artifact of the named stage before hashing so the
            // report records drift. This is the failure-path canary the
            // integration tests exercise.
            if let Some(stage) = self.inject_drift.as_deref()
                && let Some(victim) = pick_first_artifact_for_stage(&artifacts, stage)
            {
                inject_drift_byte(victim).with_context(|| {
                    format!(
                        "injecting drift byte into {} on run {}",
                        victim.display(),
                        run_idx
                    )
                })?;
            }
            per_run_hashes.push(hash_artifacts(worktree.path(), &artifacts)?);
            // Worktree dropped at end of scope → cleanup automatic.
        }

        Ok(self.build_report(per_run_hashes))
    }

    /// Construct the env map handed to each child build process. See the
    /// module doc for the policy summary.
    fn build_isolated_env(&self, worktree: &Worktree) -> Result<HashMap<String, String>> {
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
        }))
    }

    /// Shell to the running `anodize` binary inside the worktree.
    ///
    /// Delegates to [`anodizer_core::determinism_runner`] — `crates/cli/**`
    /// is on the forbid-list in `.claude/rules/module-boundaries.md`, so
    /// the actual `Command::new` lives in core where subprocess spawn is
    /// allow-listed.
    ///
    /// The `--skip=<list>` argument is load-bearing: it pins the harness
    /// to build-side stages even if a future operator extends the
    /// pipeline. The skip list is encoded in
    /// [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`] (a
    /// single source of truth — adding a new side-effect stage to
    /// `pipeline.rs` MUST register it there), not here, so every harness
    /// invocation gets the same hermetic guarantee.
    fn run_build_pipeline(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let exe = anodizer_core::determinism_runner::current_anodize_binary()?;
        anodizer_core::determinism_runner::run_build_pipeline_subprocess(&exe, worktree_path, env)
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
            let allow_reason = self.resolve_allow_reason(name);

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
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: None,
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

/// Inputs for [`build_subprocess_env`]. Bundled so the function signature
/// doesn't grow more positional arguments every time we add an isolated-
/// path knob.
pub(crate) struct BuildSubprocessEnv<'a> {
    pub cargo_home: &'a Path,
    pub cargo_target: &'a Path,
    pub tmpdir: &'a Path,
    pub home_dir: &'a Path,
    pub sde: i64,
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

    // Inherit only the explicit allow-list of identity-only host env so
    // build scripts that conditionally embed git/CI info still work, and
    // no credential-bearing vars (GITHUB_TOKEN, ACTIONS_RUNTIME_TOKEN,
    // etc.) leak into the child.
    for &key in HARNESS_ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    // Always set CI=true so build scripts know they're in a sealed env.
    env.entry("CI".into()).or_insert_with(|| "true".into());

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
}

/// Conservative PATH for harness children — `/usr/bin`, `/bin`, and
/// `$HOME/.cargo/bin` when `HOME` is set (rustup-managed cargo lives
/// there on most installs).
fn allow_listed_path() -> String {
    let mut parts: Vec<String> = vec!["/usr/bin".into(), "/bin".into()];
    // Prepend the rustup cargo bin so `cargo` / `rustc` resolve to the
    // pinned toolchain rather than a system package.
    if let Ok(home) = std::env::var("HOME") {
        parts.insert(0, format!("{}/.cargo/bin", home));
    }
    parts.join(":")
}

/// Walk `<worktree>/dist` and collect every regular file. Sorted by path
/// for deterministic iteration order in tests.
fn discover_artifacts(worktree_path: &Path) -> Result<Vec<PathBuf>> {
    let dist = worktree_path.join("dist");
    let mut out = Vec::new();
    if !dist.exists() {
        return Ok(out);
    }
    visit_dir(&dist, &mut out)?;
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

/// SHA256 every artifact and return `{basename -> info}`. Map keyed on
/// basename matches the spec's allow-list pattern semantics (glob/exact
/// matches operate on the file name, not the full path).
fn hash_artifacts(
    worktree_path: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<String, ArtifactInfo>> {
    use sha2::{Digest, Sha256};
    let mut out = BTreeMap::new();
    for p in paths {
        let bytes =
            std::fs::read(p).with_context(|| format!("reading artifact {}", p.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = format!("sha256:{:x}", hasher.finalize());
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let relative = p
            .strip_prefix(worktree_path)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned();
        let stage = infer_stage_from_path(&relative);
        out.insert(
            name,
            ArtifactInfo {
                hash: digest,
                size_bytes: bytes.len() as u64,
                relative_path: relative,
                stage,
            },
        );
    }
    Ok(out)
}

/// Best-effort stage attribution from the artifact path. The harness
/// does not have access to the pipeline's per-stage Artifact records (it
/// shells to a child process), so it infers from filename extension and
/// path conventions. Falls back to `"unknown"` when nothing matches.
fn infer_stage_from_path(rel: &str) -> String {
    let lower = rel.to_lowercase();
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
/// Source byte: `/dev/urandom` on platforms that expose it; constant
/// `0xAB` fallback otherwise. The fallback is acceptable because the
/// test harness only needs the appended byte to differ from the
/// original file's last byte (or to extend the file at all) — the goal
/// is to break the hash, not to inject true entropy. Reading a single
/// byte from `/dev/urandom` on Unix is the path actually used in CI.
fn inject_drift_byte(path: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let byte: u8 = match std::fs::OpenOptions::new().read(true).open("/dev/urandom") {
        Ok(mut f) => {
            let mut buf = [0u8; 1];
            f.read_exact(&mut buf).ok();
            buf[0]
        }
        Err(_) => 0xAB,
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
                    map.insert(
                        name.into(),
                        ArtifactInfo {
                            hash: digest,
                            size_bytes: bytes.len() as u64,
                            relative_path: format!("dist/{}", name),
                            stage: infer_stage_from_path(name),
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
    }

    #[test]
    fn matches_artifact_pattern_handles_glob_and_exact() {
        assert!(matches_artifact_pattern("*.crate", "foo.crate"));
        assert!(!matches_artifact_pattern("*.crate", "foo.tar.gz"));
        assert!(matches_artifact_pattern("exact.bin", "exact.bin"));
        assert!(!matches_artifact_pattern("exact.bin", "other.bin"));
    }

    #[test]
    fn allow_listed_path_includes_usr_bin_and_bin() {
        let p = allow_listed_path();
        assert!(p.contains("/usr/bin"));
        assert!(p.contains("/bin"));
    }

    #[test]
    fn stage_id_round_trips_to_string() {
        assert_eq!(StageId::Build.as_str(), "build");
        assert_eq!(StageId::Archive.as_str(), "archive");
        assert_eq!(StageId::Sbom.as_str(), "sbom");
        assert_eq!(StageId::Sign.as_str(), "sign");
        assert_eq!(StageId::Checksum.as_str(), "checksum");
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
    }
}
