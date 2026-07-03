//! Canonical basenames for the sidecar manifests anodizer writes into the
//! `dist/` tree (and the per-`run-<id>/` subdir).
//!
//! Every writer, reader, GitHub-release uploader, split/merge loader, and the
//! determinism-harness preserve allow-list reference these constants instead of
//! repeating the literal. A sidecar rename is then a single-line edit that the
//! compiler propagates everywhere — the writer can never drift from the preserve
//! allow-list that decides which sidecars survive the shard merge (a silent,
//! one-way-release-fatal failure mode when the two disagree).
//!
//! Mirrors the single-source pattern of
//! [`crate::config::attestation::AttestationConfig::SUBJECTS_MANIFEST_NAME`].

/// `dist/metadata.json` — project metadata (name, tag, version, commit, …).
pub const METADATA_JSON: &str = "metadata.json";

/// `dist/artifacts.json` — the per-artifact manifest array.
pub const ARTIFACTS_JSON: &str = "artifacts.json";

/// `dist/context.json` — the preserved-dist context the publish-only path reads.
pub const CONTEXT_JSON: &str = "context.json";

/// `dist/run-<id>/report.json` — the publish run's replay report.
pub const REPORT_JSON: &str = "report.json";

/// `dist/run-<id>/rollback.json` — the rollback replay's updated state.
pub const ROLLBACK_JSON: &str = "rollback.json";

/// `dist/run-<id>/summary.json` — the per-run publish summary.
pub const SUMMARY_JSON: &str = "summary.json";

/// Directory-name prefix of the per-run `dist/run-<id>/` subdir. Shared by
/// the writer (`run_dir` in the publish stage) and the run-summary scanner
/// so a prefix rename cannot make the scanner silently return empty — which
/// would strip `tag rollback`'s published-state guard and every run-summary
/// display of all on-disk publish evidence.
pub const RUN_DIR_PREFIX: &str = "run-";

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the on-disk basenames. The whole module exists to stop a
    /// writer/reader rename from silently desyncing a sidecar; a typo in a
    /// const here would propagate that typo everywhere, so the literal
    /// values are asserted directly.
    #[test]
    fn sidecar_basenames_are_stable() {
        assert_eq!(METADATA_JSON, "metadata.json");
        assert_eq!(ARTIFACTS_JSON, "artifacts.json");
        assert_eq!(CONTEXT_JSON, "context.json");
        assert_eq!(REPORT_JSON, "report.json");
        assert_eq!(ROLLBACK_JSON, "rollback.json");
        assert_eq!(SUMMARY_JSON, "summary.json");
        assert_eq!(RUN_DIR_PREFIX, "run-");
    }
}
