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

use crate::determinism_harness::{Harness, StageId, installer_stages};
use anodizer_cli::CheckDeterminismArgs;
use anodizer_core::{
    AllowList, AllowListEntry, DeterminismState,
    git::{head_commit_hash_in, head_commit_timestamp_in, head_is_at_tag, resolve_snapshot_sde},
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
    let targets = parse_targets(args.targets.as_deref()).map_err(|e| anyhow::anyhow!(e))?;

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

    // `--preserve-dist=<path>` may be relative; resolve against the
    // repo root so the harness has an absolute target. The repo_root
    // is `current_dir`, so a relative `--preserve-dist=./preserved-dist`
    // lands at `<cwd>/preserved-dist` — what a CI step expects when
    // passing the flag verbatim.
    let preserve_dist = args.preserve_dist.as_ref().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            repo_root.join(p)
        }
    });

    // Fallback only — production runs always have a sibling metadata.json
    // that wins. A missing or malformed one would otherwise emit anodizer's
    // own version into `context.json:version`, which third-party consumers
    // would then publish as their own release version.
    let version_hint =
        read_project_version(&repo_root).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let child_snapshot =
        resolve_child_snapshot(args.snapshot, args.no_snapshot, head_is_at_tag(&repo_root)?);

    let harness = Harness {
        repo_root: repo_root.clone(),
        commit: commit.clone(),
        stages,
        runs: args.runs,
        sde,
        allowlist,
        report_path: report_path.clone(),
        inject_drift,
        targets,
        preserve_dist,
        version_hint,
        child_snapshot,
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
            // Surface `differing_bytes_summary` alongside hashes. The
            // summary is already computed by the harness and stored in
            // `determinism.json`, but the JSON only ships if the publish
            // job runs — and publish is gated on determinism passing.
            // Printing here makes the offset hint (e.g. `first diff at
            // offset 0x130`) visible directly in CI logs (90-day
            // retention), surviving even when the run's artifacts expire.
            match &d.differing_bytes_summary {
                Some(summary) => {
                    eprintln!("  - {}: {} | {:?}", d.artifact, summary, d.hashes);
                }
                None => {
                    eprintln!("  - {}: {:?}", d.artifact, d.hashes);
                }
            }
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
    // Umbrella selector for every installer-family stage. Operators
    // type `--stages=installers` to exercise the full set in one shot;
    // individual family stages (`msi`, `nsis`, ...) remain available
    // for narrower runs. Delegating to the harness's
    // `installer_detect::installer_stages` keeps the CLI parser and
    // harness gate consulting the same source of truth.
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
                    "source" => parsed.push(StageId::Source),
                    "upx" => parsed.push(StageId::Upx),
                    "archive" => parsed.push(StageId::Archive),
                    "nfpm" => parsed.push(StageId::Nfpm),
                    "makeself" => parsed.push(StageId::Makeself),
                    "snapcraft" => parsed.push(StageId::Snapcraft),
                    "sbom" => parsed.push(StageId::Sbom),
                    "sign" => parsed.push(StageId::Sign),
                    "checksum" => parsed.push(StageId::Checksum),
                    "cargo-package" => parsed.push(StageId::CargoPackage),
                    "docker" => parsed.push(StageId::Docker),
                    "msi" => parsed.push(StageId::Msi),
                    "nsis" => parsed.push(StageId::Nsis),
                    "dmg" => parsed.push(StageId::Dmg),
                    "pkg" => parsed.push(StageId::Pkg),
                    "srpm" => parsed.push(StageId::Srpm),
                    "installers" => parsed.extend(installer_stages()),
                    other => unknown.push(other.to_string()),
                }
            }
            if !unknown.is_empty() {
                return Err(format!(
                    "--stages contained unknown stage(s): {}. \
                     Known stages: build, source, upx, archive, nfpm, makeself, snapcraft, sbom, sign, checksum, cargo-package, docker, msi, nsis, dmg, pkg, srpm, installers.",
                    unknown.join(", ")
                ));
            }
            // De-dup while preserving insertion order so
            // `--stages=installers,msi` (umbrella followed by an
            // individual member) doesn't list `msi` twice in
            // `stages_under_test`. The first mention wins, matching
            // the operator's typed intent.
            let mut seen: std::collections::HashSet<StageId> = std::collections::HashSet::new();
            let mut deduped: Vec<StageId> = Vec::with_capacity(parsed.len());
            for stage in parsed {
                if seen.insert(stage) {
                    deduped.push(stage);
                }
            }
            Ok(if deduped.is_empty() {
                default()
            } else {
                deduped
            })
        }
    }
}

/// Parse a comma-separated triple list (`--targets=x86_64-...,aarch64-...`).
///
/// Thin wrapper over `commands::helpers::parse_csv_list` that supplies
/// the `--targets`-shaped error hint. Unlike `--stages=<csv>`, there is
/// no closed vocabulary to validate against here — the legal set is
/// whatever appears in the project's `.anodizer.yaml` `targets` list,
/// and that's resolved later in the pipeline.
fn parse_targets(s: Option<&str>) -> Result<Option<Vec<String>>, String> {
    crate::commands::helpers::parse_csv_list(
        s,
        "--targets=x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu",
    )
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

/// Resolve the harness's `child_snapshot` flag.
///
/// ```text
/// snapshot | no_snapshot | head_at_tag | child_snapshot | reason
/// ---------+-------------+-------------+----------------+--------
///  true    | -           | -           | true           | explicit --snapshot
///  -       | true        | -           | false          | explicit --no-snapshot
///  false   | false       | true        | false          | auto: tagged → release artifacts
///  false   | false       | false       | true           | auto: untagged → snapshot artifacts
/// ```
///
/// Free function so the matrix is unit-testable without forking git.
fn resolve_child_snapshot(snapshot: bool, no_snapshot: bool, head_at_tag: bool) -> bool {
    if snapshot {
        true
    } else if no_snapshot {
        false
    } else {
        !head_at_tag
    }
}

/// Read the target project's release version from `<repo>/Cargo.toml`.
///
/// Resolves `[workspace.package].version` first (workspace inheritance,
/// as cfgd uses to share one version across crates), then falls back to
/// `[package].version`. Returns `None` if the manifest is missing,
/// unparseable, or has neither key.
fn read_project_version(repo_root: &std::path::Path) -> Option<String> {
    let manifest = repo_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            doc.get("package")
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
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
    fn parse_stages_accepts_full_byte_stable_set() {
        // Every stage name reachable from anodizer-action's per-OS
        // determinism-stages default must parse cleanly. Drift between
        // this parser and the action's expanded default surfaces as
        // "unknown stage(s): makeself, snapcraft" in CI. This test pins
        // the parser to the action's current Linux default CSV.
        let stages = parse_stages(Some(
            "build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum",
        ))
        .expect("all stages in the action's Linux default must parse");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec![
                "build",
                "source",
                "upx",
                "archive",
                "nfpm",
                "makeself",
                "snapcraft",
                "sbom",
                "sign",
                "checksum"
            ]
        );
    }

    #[test]
    fn parse_stages_errors_on_unknown_token() {
        // Typos like `--stages=archve,checksum` previously filtered to
        // just `checksum` and quietly under-verified. The unknown token
        // must surface as an error naming the bad token and the legal
        // vocabulary.
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
    fn parse_stages_installers_umbrella_expands_to_full_set() {
        // `--stages=installers` is the operator-facing shorthand for
        // every installer-family stage. The expansion must include
        // nfpm + makeself + srpm + msi + nsis + dmg + pkg in the same
        // order `installer_stages()` advertises so the harness gate
        // and the parser stay aligned.
        let stages = parse_stages(Some("installers")).expect("umbrella token must parse");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["nfpm", "makeself", "srpm", "msi", "nsis", "dmg", "pkg"]
        );
    }

    #[test]
    fn parse_stages_installers_dedupes_against_individual_members() {
        // `--stages=installers,msi` must not double-list `msi` in the
        // report's `stages_under_test`. First mention wins so the
        // operator's typed order is preserved.
        let stages =
            parse_stages(Some("installers,msi")).expect("umbrella + individual must parse");
        let names: Vec<&str> = stages.iter().map(|s| s.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "msi").count(), 1);
    }

    #[test]
    fn parse_stages_accepts_each_individual_installer_token() {
        // Every individual installer stage token must parse in
        // isolation so operators can narrow the harness to a single
        // family (`--stages=msi`) without invoking the umbrella.
        for token in ["msi", "nsis", "dmg", "pkg", "srpm"] {
            let stages = parse_stages(Some(token))
                .unwrap_or_else(|e| panic!("token `{token}` must parse: {e}"));
            assert_eq!(
                stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                vec![token]
            );
        }
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
    fn parse_targets_default_is_none() {
        assert_eq!(parse_targets(None).unwrap(), None);
    }

    #[test]
    fn parse_targets_subset_filters_to_named_list() {
        let got = parse_targets(Some("x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu"))
            .expect("ascii triples accepted");
        assert_eq!(
            got,
            Some(vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "aarch64-unknown-linux-gnu".to_string(),
            ])
        );
    }

    #[test]
    fn parse_targets_tolerates_trailing_comma_and_whitespace() {
        let got = parse_targets(Some(" x86_64-apple-darwin , aarch64-apple-darwin , "))
            .expect("trailing comma + spaces tolerated");
        assert_eq!(
            got,
            Some(vec![
                "x86_64-apple-darwin".to_string(),
                "aarch64-apple-darwin".to_string(),
            ])
        );
    }

    #[test]
    fn parse_targets_errors_on_all_empty_csv() {
        // Operator typed `--targets=""` or `--targets=", , "` — they
        // clearly meant to pass *something* but gave nothing. Silent
        // fallback to "no filter" would mask the typo and cross-compile
        // every configured target (the very bug Option B exists to
        // prevent).
        let err = parse_targets(Some("")).expect_err("empty CSV must error");
        assert!(
            err.contains("at least one entry"),
            "error must explain the requirement: {err}"
        );
        let err = parse_targets(Some(" , , ")).expect_err("whitespace-only CSV must error");
        assert!(
            err.contains("at least one entry"),
            "error must explain the requirement: {err}"
        );
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
            targets: None,
            report: None,
            snapshot: false,
            no_snapshot: false,
            inject_drift: None,
            preserve_dist: None,
        };
    }

    // ── resolve_child_snapshot ────────────────────────────────────────────

    #[test]
    fn resolve_child_snapshot_auto_off_when_head_at_tag() {
        // Tagged HEAD = cutting a release → produce-stages emit
        // release-named artifacts (no `-SNAPSHOT-<sha>` suffix). The
        // workflow's preserved-dist payload must be immediately
        // shippable via `--publish-only`.
        assert!(!resolve_child_snapshot(false, false, true));
    }

    #[test]
    fn resolve_child_snapshot_auto_on_when_head_not_at_tag() {
        // Untagged HEAD = local rehearsal → produce-stages emit
        // `-SNAPSHOT-<sha>`-suffixed artifacts so the bytes can't be
        // mistaken for a release build.
        assert!(resolve_child_snapshot(false, false, false));
    }

    #[test]
    fn resolve_child_snapshot_explicit_snapshot_beats_auto() {
        // `--snapshot` on a tagged HEAD: operator deliberately wants
        // snapshot-style artifacts even though HEAD is tagged. Auto
        // would say off; explicit must beat auto.
        assert!(resolve_child_snapshot(true, false, true));
        assert!(resolve_child_snapshot(true, false, false));
    }

    #[test]
    fn resolve_child_snapshot_explicit_no_snapshot_beats_auto() {
        // `--no-snapshot` on an untagged HEAD: legacy workflow override
        // — operator forces release-style artifact names even though
        // we're not at a tag. Auto would say on; explicit must beat
        // auto.
        assert!(!resolve_child_snapshot(false, true, false));
        assert!(!resolve_child_snapshot(false, true, true));
    }

    // ── read_project_version ──────────────────────────────────────────────

    #[test]
    fn read_project_version_returns_none_when_cargo_toml_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_project_version(tmp.path()), None);
    }

    #[test]
    fn read_project_version_reads_workspace_package_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "1.2.3-test"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(
            read_project_version(tmp.path()),
            Some("1.2.3-test".to_string())
        );
    }

    #[test]
    fn read_project_version_reads_package_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "demo"
version = "0.4.2"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(read_project_version(tmp.path()), Some("0.4.2".to_string()));
    }

    #[test]
    fn read_project_version_prefers_workspace_when_both_present() {
        // Workspace inheritance: the root `[workspace.package].version`
        // is the authoritative version and `[package].version` is
        // usually `version.workspace = true`. When both literal values
        // are present we still prefer the workspace key because that's
        // what `cargo` itself would propagate via inheritance.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[workspace.package]
version = "9.9.9"

[package]
name = "root-crate"
version = "0.0.1"
"#,
        )
        .unwrap();
        assert_eq!(read_project_version(tmp.path()), Some("9.9.9".to_string()));
    }

    #[test]
    fn read_project_version_returns_none_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "not valid \x00 toml ===").unwrap();
        assert_eq!(read_project_version(tmp.path()), None);
    }
}
