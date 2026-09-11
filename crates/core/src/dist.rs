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
//! `crate::config::attestation::AttestationConfig::SUBJECTS_MANIFEST_NAME`.

use std::path::{Path, PathBuf};

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

/// `dist/config.yaml` — the effective config every run writes before any stage
/// produces an artifact.
pub const CONFIG_YAML: &str = "config.yaml";

/// `dist/release-notes.md` — the rendered `--release-notes-tmpl`.
pub const RELEASE_NOTES_MD: &str = "release-notes.md";

/// `dist/matrix.json` — the worker matrix `release --split` writes and
/// `--merge` reconciles each shard against.
pub const MATRIX_JSON: &str = "matrix.json";

/// Directory-name prefix of the per-run `dist/run-<id>/` subdir. Shared by
/// the writer (`run_dir` in the publish stage) and the run-summary scanner
/// so a prefix rename cannot make the scanner silently return empty — which
/// would strip `tag rollback`'s published-state guard and every run-summary
/// display of all on-disk publish evidence.
pub const RUN_DIR_PREFIX: &str = "run-";

/// The directories a `dist/` tree can hold one layout's sidecars in: `dist`
/// itself, then every first-level subdirectory in sorted order.
///
/// A single-crate or lockstep run writes its sidecars and `run-<tag>/` dirs
/// straight into `dist/`; a per-crate workspace run re-anchors each crate onto
/// `dist/<crate>/` and writes them there. A reader that wants the whole run's
/// evidence probes both levels, and does it in one order so two runs over the
/// same tree observe the same sequence — `read_dir` order is undefined.
///
/// Subdirectories that hold nothing the caller wants are simply misses; this
/// answers where to LOOK, not what is there.
pub fn layout_roots(dist: &Path) -> Vec<PathBuf> {
    let mut roots = vec![dist.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(dist) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        roots.extend(subdirs);
    }
    roots
}

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

    /// The two dist layouts are probed in one fixed order — the root first,
    /// then each crate subdirectory sorted — so a reader walking the tree twice
    /// sees the same sequence and a per-crate run's evidence is never missed.
    #[test]
    fn layout_roots_lists_the_root_then_sorted_subdirs() {
        let tmp = tempfile::tempdir().expect("tempdir for dist layout");
        for name in ["zeta", "alpha", "mid"] {
            std::fs::create_dir(tmp.path().join(name)).expect("create crate subdir");
        }
        std::fs::write(tmp.path().join(ARTIFACTS_JSON), "[]").expect("write a sidecar file");

        assert_eq!(
            layout_roots(tmp.path()),
            vec![
                tmp.path().to_path_buf(),
                tmp.path().join("alpha"),
                tmp.path().join("mid"),
                tmp.path().join("zeta"),
            ]
        );
    }
}
