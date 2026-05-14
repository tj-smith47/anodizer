//! `anodize check determinism` CLI dispatcher.
//!
//! Body of the harness lives in [`crate::determinism_harness`]; this
//! module is responsible for:
//!
//! 1. Resolving the SOURCE_DATE_EPOCH from either the snapshot resolver
//!    (`--snapshot`) or the HEAD commit timestamp (default).
//! 2. Picking up the compile-time allow-list seeded by
//!    [`anodizer_core::DeterminismState::seed_from_commit`].
//! 3. Choosing the report path (CLI override → `dist/run-<commit_short>/determinism.json`).
//! 4. Invoking [`crate::determinism_harness::Harness::run`].
//! 5. Writing the report JSON and exiting non-zero on drift.
//!
//! Spec: `.claude/specs/2026-05-14-release-resilience.md#verification-harness-cli`.

use crate::determinism_harness::{Harness, StageId};
use anodizer_cli::CheckDeterminismArgs;
use anodizer_core::{
    AllowList, AllowListEntry, DeterminismState,
    git::{head_commit_hash_in, head_commit_timestamp_in, resolve_snapshot_sde},
};
use anyhow::{Context, Result};

pub fn run(args: CheckDeterminismArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("resolving repo root")?;

    // SDE source — snapshot resolver under --snapshot (handles dirty
    // tree); HEAD commit timestamp otherwise. Both routes converge on
    // an i64 "seconds since UNIX epoch" value.
    let sde = if args.snapshot {
        resolve_snapshot_sde(&repo_root)?
    } else {
        head_commit_timestamp_in(&repo_root)?
    };

    let commit = head_commit_hash_in(&repo_root)?;
    let stages = parse_stages(args.stages.as_deref());

    let report_path = args.report.clone().unwrap_or_else(|| {
        repo_root.join(format!(
            "dist/run-{}/determinism.json",
            commit_short(&commit)
        ))
    });

    // Seed the compile-time allow-list from the centralized
    // DeterminismState (single source of truth); the runtime allow-list
    // is empty here because the harness is invoked outside the
    // `release` pipeline that would have populated it.
    let state = DeterminismState::seed_from_commit(sde);
    let allowlist = AllowList {
        compile_time: state
            .compile_time_allowlist
            .iter()
            .map(|(n, r)| AllowListEntry {
                artifact: n.clone(),
                reason: r.clone(),
            })
            .collect(),
        runtime: Vec::new(),
    };

    let harness = Harness {
        repo_root: repo_root.clone(),
        commit: commit.clone(),
        stages,
        runs: args.runs,
        sde,
        allowlist,
        report_path: report_path.clone(),
    };

    let report = harness.run()?;

    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating report directory {}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(&report).context("serializing determinism report to JSON")?;
    std::fs::write(&report_path, json)
        .with_context(|| format!("writing report to {}", report_path.display()))?;
    eprintln!("Wrote determinism report to {}", report_path.display());

    if report.drift_count > 0 {
        eprintln!(
            "DRIFT DETECTED: {} artifact(s) differed across {} runs",
            report.drift_count, report.runs
        );
        for d in &report.drift {
            eprintln!("  - {}: {:?}", d.artifact, d.hashes);
        }
        // Use the conventional process::exit so the gate is observable
        // from CI even if a caller wraps the binary in a script.
        std::process::exit(1);
    }

    Ok(())
}

/// Parse a comma-separated stage subset (`--stages=build,archive,...`).
/// Unknown tokens are silently dropped; an empty / all-unknown selection
/// falls back to the canonical build-side set. Spec calls out "build,
/// archive, sbom, sign, checksum" as the legal vocabulary.
fn parse_stages(s: Option<&str>) -> Vec<StageId> {
    let default = || {
        vec![
            StageId::Build,
            StageId::Archive,
            StageId::Sbom,
            StageId::Sign,
            StageId::Checksum,
        ]
    };
    match s {
        None => default(),
        Some(list) => {
            let parsed: Vec<StageId> = list
                .split(',')
                .filter_map(|tok| match tok.trim() {
                    "build" => Some(StageId::Build),
                    "archive" => Some(StageId::Archive),
                    "sbom" => Some(StageId::Sbom),
                    "sign" => Some(StageId::Sign),
                    "checksum" => Some(StageId::Checksum),
                    _ => None,
                })
                .collect();
            if parsed.is_empty() { default() } else { parsed }
        }
    }
}

/// Truncate a commit hash to the conventional 7-char "short" form, used
/// in the default `dist/run-<short>/determinism.json` path.
fn commit_short(commit: &str) -> String {
    if commit.len() >= 7 {
        commit[..7].to_string()
    } else {
        commit.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stages_default_returns_full_build_side_set() {
        let stages = parse_stages(None);
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["build", "archive", "sbom", "sign", "checksum"]
        );
    }

    #[test]
    fn parse_stages_subset_filters_to_named_set() {
        let stages = parse_stages(Some("archive,checksum"));
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_drops_unknown_tokens_and_trims_whitespace() {
        let stages = parse_stages(Some(" archive , bogus, checksum "));
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_all_unknown_falls_back_to_default() {
        let stages = parse_stages(Some("bogus,nope"));
        assert_eq!(stages.len(), 5);
    }

    #[test]
    fn commit_short_truncates_to_seven_chars() {
        assert_eq!(commit_short("abcdef1234567890"), "abcdef1");
    }

    #[test]
    fn commit_short_keeps_short_commit_as_is() {
        assert_eq!(commit_short("abc"), "abc");
    }

    /// The harness body is exercised by the integration test at
    /// `crates/cli/tests/check_determinism.rs`. Argument-plumbing
    /// behavior is covered by the unit tests above.
    #[test]
    fn dispatcher_args_are_consumed() {
        // Sanity guard: if the CheckDeterminismArgs surface grows new
        // required fields, this test fails to compile and forces the
        // dispatcher above to pick up the new field explicitly.
        let _args = CheckDeterminismArgs {
            runs: 2,
            stages: None,
            report: None,
            snapshot: false,
        };
    }
}
