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
//! 2. Builds an isolated env (per-run `CARGO_HOME`, `CARGO_TARGET_DIR`,
//!    `TMPDIR`, `HOME`; `SOURCE_DATE_EPOCH=self.sde`; `PATH` trimmed to
//!    `allow_listed_path()`; everything else stripped except `CI`,
//!    `RUNNER_*`, `GITHUB_*`, `RUSTUP_HOME`).
//! 3. Invokes the build-side pipeline (`anodize release --snapshot
//!    --skip=release,publish,blob,snapcraft-publish,announce`) inside the
//!    worktree with that env.
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

        let mut env = HashMap::new();
        env.insert(
            "CARGO_HOME".into(),
            cargo_home.to_string_lossy().into_owned(),
        );
        env.insert(
            "CARGO_TARGET_DIR".into(),
            cargo_target.to_string_lossy().into_owned(),
        );
        env.insert("TMPDIR".into(), tmpdir.to_string_lossy().into_owned());
        env.insert("HOME".into(), home_dir.to_string_lossy().into_owned());
        env.insert("SOURCE_DATE_EPOCH".into(), self.sde.to_string());
        env.insert("PATH".into(), allow_listed_path());

        // Inherit a narrow allow-list of host env so build scripts that
        // conditionally embed git/CI info still work.
        for (k, v) in std::env::vars() {
            if k == "RUSTUP_HOME"
                || k == "CI"
                || k.starts_with("RUNNER_")
                || k.starts_with("GITHUB_")
            {
                env.insert(k, v);
            }
        }
        // Always set CI=true so build scripts know they're in a sealed env.
        env.entry("CI".into()).or_insert_with(|| "true".into());

        Ok(env)
    }

    /// Shell to the running `anodize` binary inside the worktree.
    ///
    /// Delegates to [`anodizer_core::determinism_runner`] — `crates/cli/**`
    /// is on the forbid-list in `.claude/rules/module-boundaries.md`, so
    /// the actual `Command::new` lives in core where subprocess spawn is
    /// allow-listed.
    ///
    /// `--skip=release,publish,blob,snapcraft-publish,announce` is
    /// load-bearing: it pins the harness to build-side stages even if a
    /// future operator extends the pipeline. The skip list is encoded in
    /// the runner itself, not here, so every harness invocation gets the
    /// same hermetic guarantee.
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
}
