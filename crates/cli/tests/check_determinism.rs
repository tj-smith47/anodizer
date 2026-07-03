//! Integration tests for `anodize check determinism`.
//!
//! The fast tests below cover the CLI surface and the harness error
//! paths that don't require a real `cargo build`. The drift-injection
//! integration test (`inject_drift_archive_reports_drift_on_minimal_workspace`)
//! synthesizes a minimal cargo workspace and exercises the full
//! harness end-to-end; it is feature-gated on `cargo` being present on
//! `PATH` and skipped (with an eprintln) otherwise to keep the suite
//! green on hosts without a Rust toolchain.
//!
//! ## Manual integration runs (not driven by `cargo test`)
//!
//! Cases not covered automatically — kept here so an operator can
//! reproduce ad-hoc:
//!
//! ### Full N-runs harness against a fixture workspace
//!
//! ```text
//! cd <fixture-workspace>
//! anodize check determinism --runs=1 --report=det.json
//! test -f det.json && jq .schema_version det.json == 1
//! ```
//!
//! ### Drift-injection round-trip (production binary)
//!
//! ```text
//! ANODIZE_TEST_HARNESS=1 anodize check determinism \
//!   --runs=2 --inject-drift=archive
//! # Expected: exit code 1, report's drift_count > 0.
//! ```
//!
//! Both flows are covered automatically by the
//! `inject_drift_archive_reports_drift_on_minimal_workspace` test below
//! plus the unit tests in `crates/cli/src/determinism_harness.rs`. The
//! manual recipes survive here for operator debugging on hosts whose
//! `cargo`/`rustup` configuration differs from CI.

use anodizer_core::DeterminismReport;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// `anodize check determinism --help` must list every flag from the
/// spec (`--runs`, `--stages`, `--report`, `--snapshot`). A regression
/// in clap surface drops this signal silently otherwise.
#[test]
fn check_determinism_help_lists_every_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--help"])
        .output()
        .expect("invoking anodize check determinism --help");

    assert!(
        output.status.success(),
        "--help exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &[
        "--runs",
        "--stages",
        "--report",
        "--snapshot",
        "--no-snapshot",
        "--preserve-dist",
    ] {
        assert!(
            stdout.contains(flag),
            "--help missing flag {}; full output: {}",
            flag,
            stdout
        );
    }
}

/// Outside a git repo the dispatcher must error cleanly when resolving
/// HEAD (not panic, not hang). This pins the early-exit path that gates
/// the harness's expensive subprocess.
#[test]
fn check_determinism_errors_cleanly_outside_git_repo() {
    let tmp = TempDir::new().unwrap();
    // `--runs 2` clears the runs>=2 gate so the dispatcher proceeds to HEAD
    // resolution, which is the path this test pins; `--runs 1` would bail at
    // the gate and pass vacuously without ever touching git.
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--runs", "2"])
        .current_dir(tmp.path())
        .output()
        .expect("invoking anodize check determinism");

    assert!(
        !output.status.success(),
        "expected non-zero exit outside a git repo; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Smoke test: `--report=<path>` is respected when the dispatcher fails
/// fast (the report dir is NOT created in the error path; the test
/// passes by virtue of the binary exiting non-zero without panicking).
/// This is the lowest-cost shape that pins "the dispatcher reaches the
/// SDE resolver". A full N-runs harness test is below
/// (`inject_drift_archive_reports_drift_on_minimal_workspace`).
#[test]
fn check_determinism_respects_report_flag_in_error_path() {
    let tmp = TempDir::new().unwrap();
    let report = tmp.path().join("custom-report.json");
    // `--runs 2` clears the runs>=2 gate so the dispatcher proceeds to the SDE
    // resolver (the path this test pins); `--runs 1` would bail at the gate
    // before the resolver is ever reached, passing vacuously.
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--runs", "2", "--report"])
        .arg(&report)
        .current_dir(tmp.path())
        .output()
        .expect("invoking anodize check determinism");

    // Non-git-repo path: must fail with a useful message, no panic.
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "binary panicked instead of erroring cleanly: {}",
        stderr
    );
}

/// `--inject-drift=<stage>` is a hidden test-harness flag — it must be
/// rejected when `ANODIZE_TEST_HARNESS=1` is not set. This guards the
/// production-release surface: an operator who accidentally types the
/// flag gets a hard error rather than silent test-mode behaviour.
#[test]
fn inject_drift_rejected_without_test_harness_env() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "1",
            "--inject-drift",
            "archive",
        ])
        .current_dir(tmp.path())
        .env_remove("ANODIZE_TEST_HARNESS")
        .output()
        .expect("invoking anodize check determinism --inject-drift");

    assert!(
        !output.status.success(),
        "expected non-zero exit when --inject-drift is set without ANODIZE_TEST_HARNESS=1"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--inject-drift") && stderr.contains("ANODIZE_TEST_HARNESS"),
        "expected error citing both --inject-drift and ANODIZE_TEST_HARNESS; got: {}",
        stderr
    );
}

/// `--inject-drift=<stage>` is hidden from `--help` output. This
/// asserts the `hide = true` clap attribute is intact so a future
/// review can't accidentally promote the flag into the public surface.
#[test]
fn inject_drift_hidden_from_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--help"])
        .output()
        .expect("invoking anodize check determinism --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("--inject-drift"),
        "--inject-drift must not appear in --help output: {}",
        stdout
    );
}

// ── Drift-injection round-trip — fast (~1s with warm cache) ──────────────
//
// A fast integration test that synthesizes a tiny cargo workspace, runs
// the harness with `--runs=2 --inject-drift=archive`, and asserts the
// report shape + `drift_count > 0`. This test does that against a minimal
// no-deps `hello-world` binary crate.
//
// Cost: dominated by the harness's per-run `cargo build --release` (×2).
// A no-deps binary builds in ~0.2-0.5s warm, ~5-10s cold. Real
// measurements on this checkout: ~0.6s end-to-end (build + archive +
// sbom + sign + checksum × 2 runs + worktree setup + JSON serdes). Cold
// CI runs without a rustup toolchain cached will be slower but still
// well under a 30s "fast" budget.
//
// Skipped (with a `cargo test` warning line) when `cargo`/`git` aren't
// on PATH so the suite stays green on minimal hosts.

mod common;
use common::{bootstrap_minimal_cargo_repo, host_triple, run_git, tool_on_path};

/// End-to-end drift-injection integration test (I12). Synthesizes a
/// minimal cargo workspace, drives the harness with `--runs=2
/// --inject-drift=archive`, and asserts the JSON report records drift.
///
/// Requires `cargo` and `git` on PATH (the harness spawns both). Asserts their
/// presence and fails loud rather than skipping — a determinism test that
/// reports green on a toolless host is exactly the false coverage this suite
/// exists to prevent.
#[test]
fn inject_drift_archive_reports_drift_on_minimal_workspace() {
    assert!(
        tool_on_path("cargo") && tool_on_path("git"),
        "inject_drift_archive_reports_drift_on_minimal_workspace requires cargo and git on PATH"
    );

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-det-fixture");

    // RUSTUP_HOME / PATH propagation is the harness's responsibility —
    // `build_subprocess_env` defaults RUSTUP_HOME from the host's
    // HOME/USERPROFILE when unset, and `allow_listed_path` inherits the
    // host PATH verbatim. No per-test workaround needed.

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive",
            "--inject-drift",
            "archive",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        .env("ANODIZE_TEST_HARNESS", "1")
        .output()
        .expect("invoking anodize check determinism");

    // Non-zero exit when drift is detected (the dispatcher calls
    // `process::exit(1)` after writing the report).
    assert!(
        !output.status.success(),
        "expected non-zero exit on drift; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        report_path.exists(),
        "report file missing at {}; stderr was: {}",
        report_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = fs::read_to_string(&report_path).unwrap();
    let report: DeterminismReport =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parsing report JSON: {e}\n{json}"));

    assert_eq!(report.schema_version, 1, "schema_version pinned at 1");
    assert_eq!(report.runs, 2, "harness ran exactly --runs=2 times");
    assert!(
        report.drift_count > 0,
        "expected drift_count > 0 after --inject-drift=archive; report: {:?}\nstderr: {}",
        report,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !report.drift.is_empty(),
        "drift list non-empty alongside drift_count > 0"
    );

    // Sanity: at least one drift row is the archive itself (the target
    // of `--inject-drift=archive`). The other rows are transitive —
    // artifacts that record the archive's hash (e.g. metadata.json,
    // checksums.txt) propagate the byte-flip.
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.ends_with(".tar.gz") || d.artifact.ends_with(".zip")),
        "at least one drift row should be an archive artifact; got: {:?}",
        report.drift.iter().map(|d| &d.artifact).collect::<Vec<_>>()
    );
}

/// Regression pin for the v0.9.0 release failure: the harness's replica
/// pipelines run on credential-less runners (the run paths skip what's
/// absent; nothing publishes), so the config-derived env preflight must be
/// disabled for the children by construction. The fixture mirrors the live
/// shape: HEAD at a tag (non-snapshot child), an nfpm apk signature whose
/// key env var is deliberately unset, and submitter publishers configured
/// `required: true`. Pre-fix this run aborted with
/// `Error: preflight: N check(s) failed` from determinism run 0.
///
/// Also pins the console contract:
/// - the `Checking determinism` header + parameter block print exactly once
///   (one formatter — the binary's; wrappers must not echo their own), and
/// - the submitter moderation-queue advisory is HIDDEN at the default log
///   level (verbose-only): it must not appear in this default-verbosity run
///   for either configured submitter (it would have printed 4× in the
///   pre-fix live run).
///
/// Scoped to Linux: the fixture drives the nfpm stage (deb/rpm/apk), so the
/// host needs `nfpm` — the same per-OS-installer convention as
/// `msi_is_byte_reproducible` (Windows) and `dmg_is_byte_reproducible` (macOS).
/// The env-preflight-skip logic under test is OS-agnostic, so Linux coverage is
/// sufficient. `nfpm` is provisioned on the Linux test/coverage CI jobs; if it
/// is ever absent the determinism gate hard-fails this test (never silently
/// skips), surfacing the missing tool.
#[cfg(target_os = "linux")]
#[test]
fn harness_skips_env_preflight_and_prints_header_and_config_warnings_once() {
    assert!(
        tool_on_path("cargo") && tool_on_path("git") && tool_on_path("nfpm"),
        "harness_skips_env_preflight_and_prints_header_and_config_warnings_once requires \
         cargo, git, and nfpm on PATH (the fixture drives the nfpm stage, which the \
         determinism gate hard-fails on when its tool is absent)"
    );

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-preflight-fixture");

    let host = host_triple();
    let yaml = format!(
        r#"crates:
  - name: anodize-preflight-fixture
    path: .
    tag_template: "v{{{{ Version }}}}"
    builds:
      - id: anodize-preflight-fixture
        binary: anodize-preflight-fixture
        targets:
          - {host}
    nfpms:
      - id: default
        formats:
          - apk
        maintainer: "Test <test@test.com>"
        description: "preflight fixture"
        apk:
          signature:
            key_file: "{{{{ .Env.APK_PRIVATE_KEY_PATH }}}}"
    publish:
      chocolatey:
        required: true
      winget:
        required: true
"#,
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "preflight fixture config"]);
    // HEAD at a tag → the harness auto-selects a NON-snapshot child (the
    // live tag-push shape), which is exactly the mode that ran the env
    // preflight pre-fix.
    run_git(repo, &["tag", "v0.1.0"]);

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive,nfpm",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        // The unsatisfiable preflight requirement: the apk signature's
        // key env var is absent, as on a credential-less CI runner.
        .env_remove("APK_PRIVATE_KEY_PATH")
        // Force plain output: under GITHUB_ACTIONS/CI the binary forces
        // color, and the ANSI reset between the bold section verb and
        // its message would break `matches("Checking determinism")`.
        // NO_COLOR wins over every color override.
        .env("NO_COLOR", "1")
        .output()
        .expect("invoking anodize check determinism");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "harness must not preflight-fail in a credential-less env; \
         stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    // Both env-preflight failure shapes: the bail error value keeps the
    // `preflight:` lead; the report header renders `N of M preflight
    // check(s) failed:`. A bare `preflight` needle is too wide — the
    // fixture crate name itself contains the word.
    assert!(
        !stderr.contains("preflight:") && !stderr.contains("preflight check(s) failed"),
        "no env-preflight failure may surface from replica children: {stderr}"
    );
    // Guard against a vacuous pass: the children must have actually driven
    // the build pipeline (one `Building binaries` section per run), not
    // short-circuited on the tags-at-HEAD no-op path.
    assert!(
        !stderr.contains("no release tags at HEAD"),
        "children must select the fixture crate from the tag, not no-op: {stderr}"
    );
    // Each child's build stage emits one default `built <crate>/<bin> for
    // <target>` result line per compiled binary (the `running cargo …`
    // command echo is verbose-only). The fixture declares a single binary,
    // so a non-vacuous run contributes exactly one such line per child run.
    let builds = stderr
        .matches("built anodize-preflight-fixture/anodize-preflight-fixture for ")
        .count();
    assert_eq!(
        builds, 2,
        "expected one build-result line per run (runs=2), got {builds}:\n{stderr}"
    );

    let header_count = stderr.matches("Checking determinism").count();
    assert_eq!(
        header_count, 1,
        "`Checking determinism` header must print exactly once, got {header_count}:\n{stderr}"
    );

    // The submitter moderation-queue advisory is verbose-only; this run uses
    // the default log level, so it must not surface for either submitter.
    for publisher in ["chocolatey", "winget"] {
        let needle = format!("publisher '{publisher}' submits to an external moderation queue");
        let count = stderr.matches(needle.as_str()).count();
        assert_eq!(
            count, 0,
            "moderation-queue advisory for {publisher} must be hidden at the \
             default log level, got {count}:\n{stderr}"
        );
    }
}

/// The dispatcher must surface a crate-universe name collision (same crate
/// name at two different paths) as a warning BEFORE the harness runs: the
/// harness resolves stages/producers through the deduped universe, which
/// silently drops the shadowed entry — without the warning, an operator's
/// colliding crate simply isn't checked and nothing says so.
///
/// The fixture keeps the run cheap by making every `builds[]` entry
/// `builder: prebuilt`, which short-circuits the dispatcher right after the
/// emission point ("no buildable targets") — the warning path is exercised
/// end-to-end without spawning a single rebuild.
#[test]
fn check_determinism_warns_on_crate_universe_name_collision() {
    assert!(
        tool_on_path("git"),
        "check_determinism_warns_on_crate_universe_name_collision requires git on PATH"
    );

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "det-collide-fixture");

    let host = host_triple();
    let yaml = format!(
        r#"crates:
  - name: det-collide-fixture
    path: .
    tag_template: "v{{{{ Version }}}}"
    builds:
      - id: det-collide-fixture
        binary: det-collide-fixture
        builder: prebuilt
        prebuilt:
          path: "out/det-collide-fixture"
        targets:
          - {host}
workspaces:
  - name: ws
    crates:
      - name: det-collide-fixture
        path: elsewhere
        tag_template: "v{{{{ Version }}}}"
"#,
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "collision fixture config"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--runs", "2"])
        .current_dir(repo)
        .env("NO_COLOR", "1")
        .output()
        .expect("invoking anodize check determinism");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "all-prebuilt short-circuit must exit 0; stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    // The universe collision warning, emitted by the dispatcher itself.
    assert!(
        stderr.contains("name collision with different paths"),
        "expected the crate-universe collision warning on stderr: {stderr}"
    );
    assert!(
        stderr.contains("workspace 'ws' crate 'det-collide-fixture'"),
        "warning must name the workspace and the colliding crate: {stderr}"
    );
    // Guard against a vacuous pass through some earlier exit: the run must
    // have reached the all-prebuilt short-circuit AFTER the emission point.
    assert!(
        stderr.contains("no buildable targets"),
        "expected the all-prebuilt short-circuit note: {stderr}"
    );
}

/// `-q` (the global quiet flag) must silence the harness's own output —
/// the `Checking determinism` header, the kv summary rows, and the
/// `run N of M` bullets — and propagate to the child release
/// subprocesses so their section headers are silenced too. Errors stay
/// audible (separate path: `log.error` / `render_error` are
/// unconditional), so a green quiet run produces an (almost) empty
/// stderr.
#[test]
fn quiet_flag_silences_harness_run_bullets_and_children() {
    assert!(
        tool_on_path("cargo") && tool_on_path("git"),
        "quiet_flag_silences_harness_run_bullets_and_children requires cargo and git on PATH"
    );

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-quiet-fixture");

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "-q",
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        // Plain output so absence assertions can't be confused by ANSI
        // styling inserted under CI-forced color.
        .env("NO_COLOR", "1")
        .output()
        .expect("invoking anodize -q check determinism");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "quiet run must still succeed; stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        report_path.exists(),
        "quiet run must still write the report"
    );
    for needle in [
        "Checking determinism",
        "run 1 of 2",
        "Building binaries",
        "running cargo",
        "wrote determinism report",
    ] {
        assert!(
            !stderr.contains(needle),
            "-q must silence `{needle}`; stderr:\n{stderr}"
        );
    }
}
