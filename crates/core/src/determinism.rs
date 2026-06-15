//! SOURCE_DATE_EPOCH seeding + compile-time / runtime allow-list state.
//!
//! `DeterminismState` is the per-run home for:
//! - `sde`: the SOURCE_DATE_EPOCH value (seconds since epoch) that every
//!   stage exports into subprocess env so artifacts have deterministic
//!   timestamps.
//! - `compile_time_allowlist`: artifact-name -> reason pairs known at
//!   build time (tool-bug allow-lists for cargo .crate, docker manifest
//!   descriptors, etc.).
//! - `runtime_allowlist`: operator-supplied opt-outs via the
//!   `--allow-nondeterministic <name>=<reason>` CLI flag.
//!
//! Both lists are surfaced into the run-summary JSON
//! (`determinism_allowlist.compile_time` and `.runtime`) and the
//! per-artifact `PublishEvidence.nondeterministic` field. On collision
//! between the two lists, the compile-time reason wins on the per-
//! artifact field; both entries still appear in the report so the
//! audit trail is complete.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterminismState {
    pub sde: i64,
    pub compile_time_allowlist: Vec<(String, String)>,
    pub runtime_allowlist: Vec<(String, String)>,
}

impl DeterminismState {
    /// Seed from a commit timestamp (seconds since UNIX epoch). All built-
    /// in compile-time allow-list entries listed in the spec's contract
    /// table are added here.
    ///
    /// Returns `Err` when `commit_ts` is negative — a negative epoch would
    /// propagate a bogus `SOURCE_DATE_EPOCH` into child processes (where
    /// shells / build tools may misinterpret it) and almost always
    /// indicates a corrupted commit graph or a test passing a sentinel
    /// like `-1`. Fail-fast is the correct UX for a determinism API.
    ///
    /// ## Compile-time allow-list scope
    ///
    /// Each entry below corresponds to an artifact pattern the
    /// [`crate::determinism_report`] verification harness will actually
    /// see in `dist/`. Entries are matched by `*.ext` suffix or exact
    /// filename against the basename of every file the harness walks
    /// under the per-run worktree's `dist/` tree. Pattern names that do
    /// not match any real emitter output are dead code (silently never
    /// resolve) — keep this list aligned with what stages actually drop
    /// into `dist/`.
    ///
    /// Notably absent (and intentionally so):
    ///
    /// - `docker-manifest-descriptor` / `docker-image-blob`: the docker
    ///   stage is in [`crate::determinism_runner::SIDE_EFFECT_STAGES`]
    ///   and skipped by the harness; the only docker file that lands in
    ///   `dist/` is a `.digest` text file written by buildx (a
    ///   deterministic sha256). No need for an allow-list entry.
    /// - `apple-notarization-receipt`: the notarize stage mutates
    ///   existing artifacts in-place (staples) rather than emitting new
    ///   files; no separate "receipt" artifact lands in `dist/`.
    /// - `*.exe-nsis`: makensis writes plain `.exe` files into
    ///   `dist/windows/`; the suffix `.exe-nsis` matches nothing the
    ///   harness ever sees. NSIS-built `.exe` files only appear when
    ///   running on Windows (or under Wine), and operators can use the
    ///   runtime `--allow-nondeterministic <name>=<reason>` flag on
    ///   those releases rather than hard-coding a dead sentinel here.
    pub fn seed_from_commit(commit_ts: i64) -> Result<Self> {
        if commit_ts < 0 {
            anyhow::bail!(
                "commit_ts must be non-negative (got {}); a corrupted commit graph or future-bug? \
                 Negative SOURCE_DATE_EPOCH would propagate to child processes and be \
                 misinterpreted by shells/build tools.",
                commit_ts
            );
        }
        // Per spec contract table: these are the artifacts whose
        // deeper reproducibility work is deferred. Listed up-front so
        // every stage that consumes them sees the same allow-list.
        // Allow-listed installer formats AND their `.sha256` sidecars —
        // the sidecar hashes a non-deterministic source so the sidecar
        // itself is non-deterministic, but it's not an independent
        // determinism finding worth surfacing.
        let installer_allow: &[(&str, &str)] = &[
            (
                "*.crate",
                "cargo package non-determinism, tracked in determinism-followups",
            ),
            (
                "*.rpm",
                "rpmbuild reproducibility deferred to determinism-installers follow-up",
            ),
            (
                "*.msi",
                "wix/candle/light reproducibility deferred to determinism-installers follow-up",
            ),
            (
                "*.dmg",
                "hdiutil reproducibility deferred to determinism-installers follow-up",
            ),
            // The Linux flat-package path (xar/mkbom/cpio) IS byte-reproducible
            // — cpio dev/ino zeroed, payload mtime-pinned, xar TOC times/inode
            // normalized and the archive re-sealed (proven by
            // stage-pkg::test_flat_pkg_is_byte_reproducible_across_time). This
            // entry remains because the allowlist matches on artifact name, not
            // on producing tool, and the macOS-native `pkgbuild` path (used on
            // the macos determinism shard) is not yet proven reproducible;
            // narrowing the gate to per-tool is the determinism-installers
            // follow-up. Removing this outright would gate anodizer's release on
            // an unproven macOS path.
            (
                "*.pkg",
                "Linux flat-pkg path (xar/mkbom/cpio) is byte-reproducible (test_flat_pkg_is_byte_reproducible_across_time), but the allowlist matches on artifact name not producing tool, and the macOS-native pkgbuild path on the macos shard is not yet proven reproducible; per-tool narrowing is the determinism-installers follow-up",
            ),
            (
                "*.deb",
                "dpkg-deb reproducibility varies by version; tracked in determinism-installers",
            ),
            (
                "*.snap",
                "snapcraft pack runs deterministically when SOURCE_DATE_EPOCH propagates (harness env exports it; mksquashfs respects it via craft-parts); allowlisted as defense-in-depth in case snapcraft introduces non-mtime variance",
            ),
        ];
        // SBOMs embed identifiers that are non-reproducible by nature:
        // CycloneDX carries a random `serialNumber` UUID plus a generation
        // `metadata.timestamp`, and SPDX carries a `documentNamespace` UUID
        // plus a `created` timestamp. syft does not honor SOURCE_DATE_EPOCH
        // for the document timestamp and (per the CycloneDX/SPDX specs) the
        // serial/namespace must be unique per document, so two runs over
        // byte-identical inputs still produce differing SBOM bytes. These
        // SBOMs are excluded from the reproducibility
        // guarantee. Surfaced in the report, excluded from `drift_count`.
        // Extensions mirror `infer_stage_from_path`'s `sbom` classifier.
        let sbom_allow: &[(&str, &str)] = &[
            (
                "*.cdx.json",
                "CycloneDX SBOM embeds a random serialNumber UUID and a generation timestamp (syft does not honor SOURCE_DATE_EPOCH for it); not byte-reproducible across runs",
            ),
            (
                "*.spdx.json",
                "SPDX SBOM embeds a documentNamespace UUID and a created timestamp; not byte-reproducible across runs",
            ),
            (
                "*.sbom.json",
                "SBOM document embeds a per-document unique identifier and generation timestamp; not byte-reproducible across runs",
            ),
        ];
        let mut compile_time_allowlist: Vec<(String, String)> = Vec::new();
        for (pattern, reason) in installer_allow.iter().chain(sbom_allow) {
            compile_time_allowlist.push(((*pattern).into(), (*reason).into()));
            compile_time_allowlist.push((
                format!("{}.sha256", pattern),
                format!("derivative of {pattern}: {reason}"),
            ));
        }
        // `artifacts.json` is anodize's own dist manifest: it records the
        // `size` and `sha256` of every produced artifact. Its byte-stability
        // is exactly the conjunction of all indexed artifacts', so it can
        // only drift when (a) a real build output drifted — already caught
        // directly on that artifact — or (b) an allow-listed non-deterministic
        // artifact (SBOM, signature) drifted — intentionally excluded. It
        // therefore carries no independent determinism signal; comparing its
        // bytes only re-surfaces drift already accounted for. Exact-match so
        // no other `.json` is swept in.
        compile_time_allowlist.push((
            "artifacts.json".into(),
            "anodize dist manifest aggregating every artifact's size+digest \
             (including allow-listed non-deterministic SBOMs/signatures); a derivative \
             signal — each indexed artifact is drift-checked independently"
                .into(),
        ));

        Ok(Self {
            sde: commit_ts,
            compile_time_allowlist,
            runtime_allowlist: Vec::new(),
        })
    }

    /// Export SOURCE_DATE_EPOCH onto a `std::process::Command` so
    /// child subprocesses (cargo, tar, sbom tools, etc.) see the
    /// reproducible epoch.
    pub fn export_env(&self, cmd: &mut Command) {
        cmd.env("SOURCE_DATE_EPOCH", self.sde.to_string());
    }

    /// Resolve the allow-list reason for an artifact name. Compile-time
    /// entries win on collision per the spec's "Operator escape /
    /// Precedence on collision" section. Returns None when the artifact
    /// is not in either list.
    pub fn resolve_reason(&self, artifact: &str) -> Option<&str> {
        // Compile-time first
        for (name, reason) in &self.compile_time_allowlist {
            if matches_artifact_pattern(name, artifact) {
                return Some(reason.as_str());
            }
        }
        // Then runtime
        for (name, reason) in &self.runtime_allowlist {
            if matches_artifact_pattern(name, artifact) {
                return Some(reason.as_str());
            }
        }
        None
    }

    /// Append a runtime allow-list entry. Caller is the CLI flag
    /// handler for `--allow-nondeterministic <name>=<reason>`.
    pub fn append_runtime(&mut self, artifact: String, reason: String) {
        self.runtime_allowlist.push((artifact, reason));
    }
}

/// Simple glob: `*.ext` matches any artifact ending in `.ext`;
/// exact-match otherwise. Avoids pulling a globbing crate for this
/// narrow case.
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sde_from_commit_timestamp_is_idempotent() {
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s.sde, 1_715_000_000);
        let s2 = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s, s2);
    }

    #[test]
    fn compile_time_allowlist_resolves_for_cargo_crate() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        let reason = s
            .resolve_reason("anodizer-0.2.1.crate")
            .expect("matches *.crate");
        assert!(reason.contains("cargo package"));
    }

    #[test]
    fn compile_time_allowlist_resolves_for_rpm() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        assert!(s.resolve_reason("foo-1.0.rpm").is_some());
    }

    #[test]
    fn compile_time_allowlist_resolves_for_pkg() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The macOS-native pkgbuild path on the macos shard is not yet proven
        // reproducible; the harness must not count its `.pkg` as drift.
        assert!(s.resolve_reason("anodizer-0.2.1.pkg").is_some());
    }

    #[test]
    fn compile_time_allowlist_resolves_for_sbom_documents() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // syft-generated CycloneDX/SPDX SBOMs carry a random serial/namespace
        // UUID + generation timestamp and can never be byte-identical across
        // runs; the harness must not count them as drift.
        for name in [
            "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json",
            "cfgd-0.4.0-linux-amd64.tar.gz.spdx.json",
            "cfgd-0.4.0-linux-amd64.tar.gz.sbom.json",
        ] {
            assert!(
                s.resolve_reason(name).is_some(),
                "SBOM document {name} must be allow-listed"
            );
        }
    }

    #[test]
    fn compile_time_allowlist_resolves_for_sbom_checksum_sidecars() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The `.sha256` sidecar hashes the non-deterministic SBOM, so it is
        // itself non-deterministic — allow-listed as a derivative.
        let reason = s
            .resolve_reason("cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256")
            .expect("matches *.cdx.json.sha256");
        assert!(reason.contains("derivative of"));
    }

    #[test]
    fn compile_time_allowlist_resolves_for_artifacts_manifest() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        assert!(
            s.resolve_reason("artifacts.json").is_some(),
            "the dist manifest aggregates non-deterministic artifact sizes/digests"
        );
        // Exact-match: must not sweep in unrelated `.json` files.
        assert!(s.resolve_reason("config.json").is_none());
        assert!(s.resolve_reason("metadata.json").is_none());
    }

    #[test]
    fn nondeterministic_allowlist_compile_time_wins_on_collision() {
        let mut s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // Runtime entry shadowing a compile-time pattern. Compile-time
        // wins so the report shows the deeper rationale.
        s.append_runtime(
            "*.crate".into(),
            "operator escape (wrong runtime reason)".into(),
        );
        let reason = s.resolve_reason("anodizer-0.2.1.crate").unwrap();
        assert!(
            reason.contains("cargo package"),
            "compile-time reason takes precedence"
        );
    }

    #[test]
    fn nondeterministic_allowlist_serializes_with_both_categories() {
        let mut s = DeterminismState::seed_from_commit(0).expect("non-negative");
        s.append_runtime("foo.bin".into(), "tool-bug-1234".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("compile_time_allowlist"));
        assert!(json.contains("runtime_allowlist"));
        assert!(json.contains("foo.bin"));
    }

    #[test]
    fn export_env_sets_source_date_epoch() {
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        let mut cmd = Command::new("true");
        s.export_env(&mut cmd);
        let env_vars: Vec<(_, _)> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
            .collect();
        let sde_entry = env_vars.iter().find(|(k, _)| k == "SOURCE_DATE_EPOCH");
        assert!(sde_entry.is_some());
        assert_eq!(sde_entry.unwrap().1, "1715000000");
    }

    #[test]
    fn resolve_reason_returns_none_for_unrecognized() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        assert!(s.resolve_reason("unrelated.txt").is_none());
    }

    #[test]
    fn seed_from_commit_accepts_zero() {
        // Epoch zero (1970-01-01) is a legitimate sentinel — some
        // determinism modes anchor SDE to UNIX epoch when the commit
        // graph isn't usable. Must not be rejected.
        let s = DeterminismState::seed_from_commit(0).expect("zero is non-negative");
        assert_eq!(s.sde, 0);
    }

    #[test]
    fn seed_from_commit_accepts_positive() {
        // Typical real-world commit timestamp.
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s.sde, 1_715_000_000);
    }

    #[test]
    fn seed_from_commit_rejects_negative() {
        let err = DeterminismState::seed_from_commit(-1).expect_err("negative must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-negative") && msg.contains("-1"),
            "error must name the bad input and the constraint: {msg}"
        );
    }
}
