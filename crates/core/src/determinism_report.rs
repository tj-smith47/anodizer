//! Determinism harness report types.
//!
//! `DeterminismReport` is the canonical JSON shape emitted by
//! `anodize check determinism` at
//! `dist/run-<commit>/determinism.json`. The shape is fixed by the
//! release-resilience spec ([determinism harness report]) — every
//! field is consumed by downstream CI parsers, so the serde contract is
//! load-bearing:
//!
//! - `schema_version: 1` (constant; bump only on a breaking shape change).
//! - `#[serde(deny_unknown_fields)]` enforced on every struct so a typo'd
//!   field in a downstream-edited report fails loudly instead of being
//!   silently dropped.
//!
//! These types live in `anodizer-core` (not the CLI crate) so future CI
//! parsers can deserialize the report without pulling in the entire CLI
//! dependency tree.
//!
//! [determinism harness report]:
//!   ../../../.claude/specs/2026-05-14-release-resilience.md#verification-harness-report

use serde::{Deserialize, Serialize};

/// Current schema version emitted by the harness. Bump on any breaking
/// field rename or removal; deserialization callers should match on this
/// before consuming the rest of the payload.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Top-level determinism report shape.
///
/// Emitted at `dist/run-<commit>/determinism.json` after every
/// `anodize check determinism` run. Non-zero exit accompanies a non-empty
/// `drift` list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeterminismReport {
    /// Schema version — currently `1`. See [`CURRENT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// `anodize` crate version that produced the report.
    pub anodize_version: String,
    /// Full commit SHA of HEAD at harness invocation time.
    pub commit: String,
    /// Committer timestamp (seconds since UNIX epoch) of `commit`. In
    /// `--snapshot` mode this is the resolved snapshot-SDE, which may
    /// differ from the raw commit timestamp when the tree is dirty.
    pub commit_timestamp: i64,
    /// Number of from-clean rebuilds the harness performed.
    pub runs: u32,
    /// Ordered list of stage names actually exercised (e.g.
    /// `["build", "archive", "sbom", "sign", "checksum"]`).
    pub stages_under_test: Vec<String>,
    /// Compile-time and runtime allow-lists carried through from
    /// [`crate::DeterminismState`].
    pub allowlist: AllowList,
    /// Per-artifact row, one entry per distinct artifact name seen across
    /// any run. Includes both deterministic and drifting artifacts.
    pub artifacts: Vec<ArtifactRow>,
    /// Drift rows — one entry per artifact whose SHA256 differed across
    /// runs AND was NOT covered by `allowlist`. Empty when the harness
    /// passes.
    pub drift: Vec<DriftRow>,
    /// `drift.len() as u32`, hoisted to a top-level field so CI parsers
    /// can short-circuit on the integer without walking the array.
    pub drift_count: u32,
}

/// Compile-time + runtime allow-list pair, mirroring
/// [`crate::DeterminismState::compile_time_allowlist`] /
/// [`crate::DeterminismState::runtime_allowlist`].
///
/// `#[serde(default)]` so an absent `allowlist` field deserializes to an
/// empty pair instead of erroring; harness emits the field always.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AllowList {
    /// Compile-time entries seeded by [`crate::DeterminismState::seed_from_commit`].
    pub compile_time: Vec<AllowListEntry>,
    /// Runtime entries added via `anodize release --allow-nondeterministic`.
    pub runtime: Vec<AllowListEntry>,
}

/// One allow-list entry: an artifact name (or `*.ext` glob) and the
/// operator-facing reason it is exempt from drift counting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AllowListEntry {
    /// Artifact name or `*.ext` glob (see
    /// [`crate::DeterminismState`] for pattern semantics).
    pub artifact: String,
    /// Human-readable reason surfaced into the report so consumers can
    /// audit the rationale alongside the SHA256SUMS file.
    pub reason: String,
}

/// One row per emitted artifact.
///
/// `deterministic=true` artifacts carry a single `hash`; drifting
/// artifacts carry the per-run array under `hashes` (and may still have
/// `nondeterministic_reason` set when allow-listed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRow {
    /// File name as emitted (basename of the artifact path).
    pub name: String,
    /// Path as seen by the harness — workspace-relative when possible,
    /// absolute otherwise.
    pub path: String,
    /// Size in bytes, taken from the last run that produced the artifact.
    pub size_bytes: u64,
    /// Stage name responsible for the artifact (e.g. `archive`, `sbom`).
    /// Best-effort — the harness infers from output path conventions and
    /// falls back to `"unknown"` when it cannot attribute.
    pub stage: String,
    /// `true` when every run produced an identical SHA256.
    pub deterministic: bool,
    /// Set when the artifact is on the allow-list. Drives the
    /// "allowlist excluded this from drift_count" UX.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nondeterministic_reason: Option<String>,
    /// Single hash when the artifact is deterministic; `None` otherwise.
    /// Mutually exclusive with `hashes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Per-run hash array when the artifact drifted (length == runs).
    /// `skip_serializing_if = "Vec::is_empty"` keeps the JSON compact for
    /// deterministic rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hashes: Vec<String>,
}

/// One drift entry. Mirrors the spec's example shape:
/// `{ artifact, hashes, differing_bytes_summary? }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DriftRow {
    /// Artifact name (matches the corresponding `ArtifactRow.name`).
    pub artifact: String,
    /// Per-run SHA256 hashes that differed.
    pub hashes: Vec<String>,
    /// Optional human-readable summary of where the bytes diverge (e.g.
    /// `"tar entry mtimes differ at offset 0x1234"`). Heuristic; the
    /// harness emits `None` when it cannot localize the drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub differing_bytes_summary: Option<String>,
    /// Base64-encoded head-sample bytes per run. Each entry pairs with
    /// the corresponding `hashes[i]`. Populated only when drift is
    /// detected, so operators can decode and diff the raw bytes around
    /// the divergence point without needing to re-run the harness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub head_samples_b64: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> DeterminismReport {
        DeterminismReport {
            schema_version: CURRENT_SCHEMA_VERSION,
            anodize_version: "0.2.1".into(),
            commit: "abc123".into(),
            commit_timestamp: 1_715_000_000,
            runs: 2,
            stages_under_test: vec!["archive".into(), "checksum".into()],
            allowlist: AllowList {
                compile_time: vec![AllowListEntry {
                    artifact: "anodizer-0.2.1.crate".into(),
                    reason: "cargo package non-determinism".into(),
                }],
                runtime: vec![],
            },
            artifacts: vec![
                ArtifactRow {
                    name: "anodizer_0.2.1_linux_amd64.tar.gz".into(),
                    path: "dist/anodizer_0.2.1_linux_amd64.tar.gz".into(),
                    size_bytes: 5_242_880,
                    stage: "archive".into(),
                    deterministic: true,
                    nondeterministic_reason: None,
                    hash: Some("sha256:abc".into()),
                    hashes: vec![],
                },
                ArtifactRow {
                    name: "anodizer-0.2.1.crate".into(),
                    path: "dist/anodizer-0.2.1.crate".into(),
                    size_bytes: 1_048_576,
                    stage: "cargo-package".into(),
                    deterministic: false,
                    nondeterministic_reason: Some("cargo package non-determinism".into()),
                    hash: None,
                    hashes: vec!["sha256:a".into(), "sha256:b".into()],
                },
            ],
            drift: vec![],
            drift_count: 0,
        }
    }

    #[test]
    fn report_roundtrips_through_json() {
        let r = sample_report();
        let s = serde_json::to_string(&r).unwrap();
        let back: DeterminismReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn schema_version_constant_is_one() {
        assert_eq!(CURRENT_SCHEMA_VERSION, 1);
    }

    #[test]
    fn deterministic_row_skips_hashes_array_in_json() {
        let r = sample_report();
        let s = serde_json::to_string(&r).unwrap();
        // First artifact is deterministic — should NOT serialize a
        // `hashes` array (the array would imply per-run drift).
        let first = &r.artifacts[0];
        assert!(first.hashes.is_empty());
        assert!(
            !s.contains("\"hashes\":[]"),
            "deterministic rows must omit empty hashes array, got: {}",
            s
        );
    }

    #[test]
    fn nondeterministic_row_skips_singular_hash_field_in_json() {
        let r = sample_report();
        // Second artifact (nondeterministic) has `hash: None`.
        let second = &r.artifacts[1];
        assert!(second.hash.is_none());
        let s = serde_json::to_string(&r).unwrap();
        // The `hash` key must not appear with a null value on the second
        // artifact.
        let second_segment = s.split("anodizer-0.2.1.crate").nth(1).unwrap();
        assert!(
            !second_segment.contains("\"hash\":null"),
            "nondeterministic rows must omit null hash field, got: {}",
            s
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let s = r#"{
            "schema_version": 1,
            "anodize_version": "0.2.1",
            "commit": "abc",
            "commit_timestamp": 0,
            "runs": 1,
            "stages_under_test": [],
            "allowlist": { "compile_time": [], "runtime": [] },
            "artifacts": [],
            "drift": [],
            "drift_count": 0,
            "bogus_field": "should reject"
        }"#;
        let res: Result<DeterminismReport, _> = serde_json::from_str(s);
        assert!(
            res.is_err(),
            "deny_unknown_fields must reject the bogus_field"
        );
    }

    #[test]
    fn unknown_fields_rejected_on_allowlist_entry() {
        let s = r#"{
            "schema_version": 1,
            "anodize_version": "0.2.1",
            "commit": "abc",
            "commit_timestamp": 0,
            "runs": 1,
            "stages_under_test": [],
            "allowlist": {
                "compile_time": [
                    {"artifact": "x", "reason": "y", "extra": "boom"}
                ],
                "runtime": []
            },
            "artifacts": [],
            "drift": [],
            "drift_count": 0
        }"#;
        let res: Result<DeterminismReport, _> = serde_json::from_str(s);
        assert!(res.is_err(), "AllowListEntry must reject unknown fields");
    }

    #[test]
    fn drift_row_with_optional_summary_serializes() {
        let d = DriftRow {
            artifact: "foo.tar.gz".into(),
            hashes: vec!["sha256:1".into(), "sha256:2".into()],
            differing_bytes_summary: Some("tar mtime offset 0x100".into()),
            head_samples_b64: vec![],
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains("differing_bytes_summary"));
        let back: DriftRow = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn drift_row_omits_summary_when_none() {
        let d = DriftRow {
            artifact: "foo.tar.gz".into(),
            hashes: vec!["sha256:1".into(), "sha256:2".into()],
            differing_bytes_summary: None,
            head_samples_b64: vec![],
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(!s.contains("differing_bytes_summary"));
    }
}
