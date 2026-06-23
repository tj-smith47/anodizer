//! Integration test for `--stages=docker`.
//!
//! Drives the determinism harness against a minimal Dockerfile fixture
//! to verify the OCI tarball produced by `docker buildx build
//! --rewrite-timestamp --output=type=oci,dest=…` is byte-stable across
//! runs. The harness invokes buildx inside a fresh
//! `git worktree add --detach` per run; the resulting `.oci.tar` (and
//! BuildKit-reported image digest) are copied into
//! `<worktree>/dist/docker/` so the harness's existing artifact-
//! discovery walker picks them up.
//!
//! Skips cleanly when `docker buildx` or `git` are missing from PATH so
//! the suite stays green on minimal hosts (the documentation-build job,
//! containers without docker installed, etc.). When the tools are
//! available but the harness skips internally (no Dockerfile / buildx
//! unreachable), the report still has to parse and `--stages=docker`
//! must be on the `stages_under_test` list — those two invariants are
//! pinned by the parser-side assertions even on hosts without docker.
//!
//! Known non-determinism the harness's flag set addresses:
//! - **Layer file mtimes** — `--rewrite-timestamp` rewrites every layer
//!   tar entry's mtime to `SOURCE_DATE_EPOCH` (BuildKit ≥ 0.13).
//! - **Provenance + SBOM attestations** — `--provenance=false
//!   --sbom=false` suppresses BuildKit's default attestations whose
//!   bodies embed wall-clock timestamps and BuildKit version strings.
//! - **Manifest annotations** — the harness pins the `--tag` to a
//!   deterministic constant so `org.opencontainers.image.ref.name` does
//!   not drift.
//!
//! Out of scope (the harness does NOT sign the image):
//! - **Cosign signature timestamps** — `cosign sign <image>` uploads
//!   transparency-log entries whose body embeds the signing timestamp,
//!   so signatures are non-deterministic by design. Future signed-image
//!   harness modes must pass `--tlog-upload=false` to opt out of
//!   transparency for byte-stable signatures.
//!
//! Any residual drift after these workarounds surfaces as
//! `drift_count > 0` and the test fails with the report content
//! attached. "Known broken" tolerance is not encoded — it would defeat
//! the regression detector.

use anodizer_core::DeterminismReport;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, tool_on_path};

/// `--stages=docker` must be accepted by the parser even on hosts
/// without docker installed. The harness's per-run loop then no-ops
/// when buildx is unreachable (warning through the harness logger,
/// which `-q` silences), so the
/// report still gets written with `stages_under_test` including
/// `"docker"`.
///
/// This is the lowest-cost shape that pins the parser wiring. On
/// hosts WITH docker buildx, the sibling `docker_oci_tar_is_byte_stable_…`
/// test exercises the full byte-stability path.
#[test]
fn docker_stage_token_parses_and_runs_to_completion() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP docker_stage_token_parses_and_runs_to_completion: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-docker-parse-fixture");

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "docker",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --stages=docker");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        report_path.exists(),
        "report file missing at {}; stdout={} stderr={}",
        report_path.display(),
        stdout,
        stderr
    );
    let json = fs::read_to_string(&report_path).unwrap();
    let report: DeterminismReport =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parsing report JSON: {e}\n{json}"));

    assert_eq!(report.schema_version, 1);
    assert_eq!(report.runs, 2, "harness ran exactly --runs=2 times");
    assert!(
        report.stages_under_test.iter().any(|s| s == "docker"),
        "stages_under_test must include `docker`: {:?}",
        report.stages_under_test
    );
    // Parser-path invariant: without a Dockerfile (the fixture has none)
    // the harness skips docker work entirely, so it must NOT emit any
    // `docker`-stage artifact rows and the exit must be zero.
    let docker_rows: Vec<_> = report
        .artifacts
        .iter()
        .filter(|a| a.stage == "docker")
        .collect();
    assert!(
        docker_rows.is_empty(),
        "expected no docker artifact rows when no Dockerfile is present; got {:?}",
        docker_rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(
        output.status.success(),
        "harness exited non-zero on docker-skip path; stderr={}\nreport={}",
        stderr,
        json
    );
    assert_eq!(
        report.drift_count, 0,
        "drift_count must be zero on the no-Dockerfile no-op path: {:?}",
        report.drift
    );
}

/// End-to-end byte-stability assertion for the `docker` stage.
///
/// Bootstraps a minimal binary crate plus a one-line Dockerfile,
/// drives `anodize check determinism --runs=2 --stages=docker` against
/// it, and asserts:
/// - The harness exit is zero (byte-stable across runs).
/// - The JSON report's `stages_under_test` lists `docker`.
/// - At least one artifact row has stage `docker` with
///   `deterministic = true`.
/// - The per-run hash is populated (`hash.is_some()`).
/// - Both the OCI tarball (`image.oci.tar`) AND the BuildKit-reported
///   image digest (`image.digest`) appear as separate artifact rows
///   and BOTH are byte-stable. The two are independent stability
///   signals — the tarball hash covers serialized bytes (tar member
///   ordering, manifest serialization), while the iidfile records the
///   pre-serialization manifest digest.
///
/// Skips when `docker buildx` or `git` are missing.
#[test]
fn docker_oci_tar_is_byte_stable_on_minimal_dockerfile() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP docker_oci_tar_is_byte_stable_on_minimal_dockerfile: \
             cargo or git missing from PATH"
        );
        return;
    }
    // `docker buildx version` exit 0 => buildx is reachable. Skip
    // otherwise so the test stays green on hosts without docker.
    let buildx_ok = Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !buildx_ok {
        eprintln!(
            "SKIP docker_oci_tar_is_byte_stable_on_minimal_dockerfile: \
             docker buildx not reachable"
        );
        return;
    }
    // OCI exporter probe — the default `docker` driver does NOT support
    // multi-output (it only goes to the local docker store via the
    // implicit `type=docker` exporter). The harness's reproducibility
    // story relies on `--output=type=oci,...`, which requires either the
    // `docker-container` driver or the containerd image store.
    //
    // The harness invokes docker with a redirected `HOME` (per its
    // hermeticity contract), so buildx loses the user's
    // `~/.docker/buildx/current` selection and falls back to the default
    // `docker` driver. Probe with the SAME `HOME` constraint the harness
    // uses so the skip decision matches the harness's actual capability
    // on this host. CI runners with `docker-container` set as the default
    // builder driver in `/etc/docker/buildx/...` system-wide config pass
    // the probe; local dev hosts with only a per-user `docker-container`
    // builder do not.
    let probe_dir = TempDir::new().unwrap();
    fs::write(probe_dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let probe_tar = probe_dir.path().join("probe.tar");
    let probe_home = probe_dir.path().join("empty-home");
    fs::create_dir_all(&probe_home).unwrap();
    let probe = Command::new("docker")
        .args(["buildx", "build"])
        .arg(format!(
            "--output=type=oci,dest={}",
            probe_tar.to_string_lossy()
        ))
        .arg("--tag")
        .arg("anodize/det:probe")
        .arg(probe_dir.path())
        .env_clear()
        .env("HOME", &probe_home)
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .output()
        .expect("invoking docker buildx build for OCI exporter probe");
    if !probe.status.success() {
        eprintln!(
            "SKIP docker_oci_tar_is_byte_stable_on_minimal_dockerfile: \
             docker buildx OCI exporter not available in harness-equivalent env \
             on this host (the `docker` driver requires `~/.docker/buildx/current` \
             to select a `docker-container` builder; redirected HOME loses that \
             selection so the harness cannot drive the exporter); probe stderr: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-docker-fixture");
    // A minimal scratch-based Dockerfile: no FROM that requires a pull
    // (which would fail in air-gapped CI), no COPY of build artifacts
    // (the harness drives docker BEFORE the build stage in this stage
    // selection). `scratch` is the empty base image — always available,
    // no network reach required.
    fs::write(
        repo.join("Dockerfile"),
        "FROM scratch\nLABEL anodize.fixture=det-harness\n",
    )
    .unwrap();
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["add", "Dockerfile"]).current_dir(repo);
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["commit", "-q", "-m", "add Dockerfile"])
                .current_dir(repo);
            cmd
        },
        "git",
    );

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "docker",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --stages=docker");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        report_path.exists(),
        "report file missing at {}; stdout={} stderr={}",
        report_path.display(),
        stdout,
        stderr
    );
    let json = fs::read_to_string(&report_path).unwrap();
    let report: DeterminismReport =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parsing report JSON: {e}\n{json}"));

    assert_eq!(report.schema_version, 1);
    assert_eq!(report.runs, 2);
    assert!(
        report.stages_under_test.iter().any(|s| s == "docker"),
        "stages_under_test must include `docker`: {:?}",
        report.stages_under_test
    );

    let docker_rows: Vec<_> = report
        .artifacts
        .iter()
        .filter(|a| a.stage == "docker")
        .collect();
    assert!(
        !docker_rows.is_empty(),
        "expected at least one docker artifact row; report.artifacts={:?}",
        report
            .artifacts
            .iter()
            .map(|a| (&a.name, &a.stage))
            .collect::<Vec<_>>()
    );

    // Both fingerprints must appear: OCI tarball + BuildKit image digest.
    let has_oci_tar = docker_rows
        .iter()
        .any(|r| r.name.ends_with("image.oci.tar"));
    let has_digest = docker_rows.iter().any(|r| r.name.ends_with("image.digest"));
    assert!(
        has_oci_tar,
        "missing OCI tarball artifact row; got {:?}",
        docker_rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(
        has_digest,
        "missing BuildKit image-digest companion row; got {:?}",
        docker_rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    assert!(
        output.status.success(),
        "harness exited non-zero (drift detected); stderr={}\nreport={}",
        stderr,
        json
    );
    assert_eq!(
        report.drift_count, 0,
        "drift detected in docker output; drift rows: {:?}",
        report.drift
    );
    for row in &docker_rows {
        assert!(
            row.deterministic,
            "docker row `{}` must be deterministic; hashes={:?}",
            row.name, row.hashes
        );
        assert!(
            row.hash.is_some(),
            "deterministic docker row `{}` must carry a single hash, not per-run array",
            row.name
        );
    }
}
