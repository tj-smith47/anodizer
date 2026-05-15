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

    // `--inject-drift` is a test-only flag gated by
    // `ANODIZE_TEST_HARNESS=1`. The flag is hidden from `--help`, so the
    // only way for an operator to trip the rejection branch is to type
    // it deliberately; the hard error keeps the surface from being
    // exercised accidentally on production releases.
    let inject_drift = if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") {
        args.inject_drift.clone()
    } else if args.inject_drift.is_some() {
        anyhow::bail!("--inject-drift requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)");
    } else {
        None
    };

    // SDE source — snapshot resolver under --snapshot (handles dirty
    // tree); HEAD commit timestamp otherwise. Both routes converge on
    // an i64 "seconds since UNIX epoch" value.
    let sde = if args.snapshot {
        resolve_snapshot_sde(&repo_root)?
    } else {
        head_commit_timestamp_in(&repo_root)?
    };

    let commit = head_commit_hash_in(&repo_root)?;
    let stages = parse_stages(args.stages.as_deref()).map_err(|e| anyhow::anyhow!(e))?;

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
    let state = DeterminismState::seed_from_commit(sde)
        .context("seeding determinism state from HEAD commit timestamp")?;
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
        inject_drift,
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
///
/// Returns `Err` on unknown tokens — silently dropping typos like
/// `--stages=archve,checksum` (note the missing `i`) is a UX trap that
/// quietly under-verifies the release; the operator typed a stage they
/// expected to be exercised. Empty / whitespace-only tokens (e.g. a
/// trailing comma) are tolerated. An empty selection (`--stages=""`)
/// falls back to the canonical build-side set. Spec calls out "build,
/// archive, sbom, sign, checksum" as the legal vocabulary.
fn parse_stages(s: Option<&str>) -> Result<Vec<StageId>, String> {
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
        None => Ok(default()),
        Some(list) => {
            let mut parsed: Vec<StageId> = Vec::new();
            let mut unknown: Vec<String> = Vec::new();
            for tok in list.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    // Tolerate trailing / empty tokens (e.g.
                    // `archive,checksum,`); the operator clearly meant
                    // the named stages and the empty slot is noise.
                    continue;
                }
                match tok {
                    "build" => parsed.push(StageId::Build),
                    "archive" => parsed.push(StageId::Archive),
                    "sbom" => parsed.push(StageId::Sbom),
                    "sign" => parsed.push(StageId::Sign),
                    "checksum" => parsed.push(StageId::Checksum),
                    other => unknown.push(other.to_string()),
                }
            }
            if !unknown.is_empty() {
                return Err(format!(
                    "--stages contained unknown stage(s): {}. \
                     Known stages: build, archive, sbom, sign, checksum.",
                    unknown.join(", ")
                ));
            }
            Ok(if parsed.is_empty() { default() } else { parsed })
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
        let stages = parse_stages(None).expect("None is always Ok");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["build", "archive", "sbom", "sign", "checksum"]
        );
    }

    #[test]
    fn parse_stages_subset_filters_to_named_set() {
        let stages = parse_stages(Some("archive,checksum")).expect("all known stages");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_errors_on_unknown_token() {
        // Audit M11: typos like `--stages=archve,checksum` previously
        // filtered to just `checksum` and quietly under-verified. The
        // unknown token must surface as an error naming the bad token
        // and the legal vocabulary.
        let err = parse_stages(Some(" archive , bogus, checksum "))
            .expect_err("unknown token must error");
        assert!(
            err.contains("bogus") && err.contains("Known stages"),
            "error must name the bad token and the legal vocabulary: {err}"
        );
        // Multiple unknowns are reported together rather than failing on
        // the first — the operator gets a complete picture in one shot.
        let err = parse_stages(Some("archve,nope")).expect_err("multiple unknowns must error");
        assert!(
            err.contains("archve") && err.contains("nope"),
            "all unknown tokens must be named: {err}"
        );
    }

    #[test]
    fn parse_stages_tolerates_trailing_comma_and_whitespace() {
        // Empty / whitespace-only tokens (trailing comma, double comma,
        // surrounding spaces) are noise rather than typos.
        let stages = parse_stages(Some("archive,checksum,")).expect("trailing comma tolerated");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
        let stages = parse_stages(Some(" archive , , checksum ")).expect("empty middle tolerated");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_empty_string_falls_back_to_default() {
        // An empty / all-whitespace selection picks the canonical build-
        // side set so `--stages=""` doesn't degrade into a no-op.
        let stages = parse_stages(Some("")).expect("empty list returns default");
        assert_eq!(stages.len(), 5);
        let stages = parse_stages(Some(" , , ")).expect("whitespace-only returns default");
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
            inject_drift: None,
        };
    }
}
