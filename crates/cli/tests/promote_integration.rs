//! Integration tests for `anodizer promote`.
//!
//! `promote::run` — config discovery, the `--from` default, the dry-run
//! preflight skip, the per-publisher summary loop and the aggregated exit
//! code — is only reachable by spawning the binary, so these spawn it against
//! a fixture `.anodizer.yaml` in each of the three config modes.
//!
//! Exiting 0 here does NOT by itself prove the dry-run spawned no `snapcraft`:
//! a host that has the tool would also exit 0. The no-spawn property is tested
//! where it can be observed — `promote_dry_run_names_plan_and_spawns_nothing`
//! in the snapcraft promoter and
//! `dry_run_dispatch_emits_would_promote_and_spawns_nothing` in the verb's own
//! dispatch. These tests check what only a spawn reaches:
//! config discovery, the `--from` default, the per-publisher summary loop and
//! the aggregated exit code.

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn anodizer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
}

/// Write `.anodizer.yaml` into a fresh temp dir and return it.
fn project(config: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(".anodizer.yaml"), config).expect("write config");
    dir
}

/// Run `anodizer promote <args>` in `dir` and return `(success, stderr)`.
fn promote(dir: &Path, args: &[&str]) -> (bool, String) {
    promote_with(anodizer(), dir, args)
}

/// Run `anodizer promote <args>` with `SOURCE_DATE_EPOCH` set to `value`.
fn promote_with_source_date(dir: &Path, value: &str, args: &[&str]) -> (bool, String) {
    let mut cmd = anodizer();
    cmd.env("SOURCE_DATE_EPOCH", value)
        .env_remove("ANODIZE_SOURCE_DATE_EPOCH");
    promote_with(cmd, dir, args)
}

fn promote_with(mut cmd: Command, dir: &Path, args: &[&str]) -> (bool, String) {
    let out = cmd
        .current_dir(dir)
        .arg("promote")
        .args(args)
        .output()
        .expect("spawn anodizer promote");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Single-crate mode: one top-level crate carrying a `snapcrafts:` block.
const SINGLE_CRATE: &str = "\
project_name: myapp
crates:
  - name: myapp
    path: .
    builds:
      - binary: myapp
    snapcrafts:
      - name: myapp
        publish: true
";

/// Lockstep mode: two top-level crates, each with its own snap.
const LOCKSTEP: &str = "\
project_name: myapp
crates:
  - name: alpha
    path: crates/alpha
    snapcrafts:
      - name: alpha
        publish: true
  - name: beta
    path: crates/beta
    snapcrafts:
      - name: beta
        publish: true
";

/// Per-crate mode: the crates live under `workspaces[].crates`.
const PER_CRATE: &str = "\
project_name: myapp
workspaces:
  - name: ws
    crates:
      - name: alpha
        path: crates/alpha
        snapcrafts:
          - name: alpha
            publish: true
      - name: beta
        path: crates/beta
        snapcrafts:
          - name: beta
            publish: true
";

/// Omitting `--from` promotes from the canonical `prerelease` track, which the
/// snapcraft promoter resolves to its native `candidate`. Checks that default
/// and the dry-run's zero exit in single-crate mode.
#[test]
fn promote_dry_run_single_crate_mode_names_default_from_track() {
    let dir = project(SINGLE_CRATE);
    let (ok, out) = promote(dir.path(), &["--to", "stable", "--dry-run"]);
    assert!(ok, "a dry-run promotion must exit 0; got:\n{out}");
    assert!(
        out.contains("(dry-run) would promote snapcraft myapp newest candidate→stable"),
        "the default --from must resolve to candidate; got:\n{out}"
    );
}

/// Lockstep workspace: every configured snap is promoted.
#[test]
fn promote_dry_run_lockstep_workspace_promotes_each_snap() {
    let dir = project(LOCKSTEP);
    let (ok, out) = promote(
        dir.path(),
        &["--to", "stable", "--publishers", "snapcraft", "--dry-run"],
    );
    assert!(ok, "a dry-run promotion must exit 0; got:\n{out}");
    assert!(
        out.contains("would promote snapcraft alpha")
            && out.contains("would promote snapcraft beta"),
        "both crates' snaps must be promoted; got:\n{out}"
    );
}

/// Per-crate workspace: the snap set is read from the crate universe, so
/// crates declared only under `workspaces[].crates` promote independently.
#[test]
fn promote_dry_run_per_crate_workspace_promotes_each_snap() {
    let dir = project(PER_CRATE);
    let (ok, out) = promote(
        dir.path(),
        &["--to", "stable", "--publishers", "snapcraft", "--dry-run"],
    );
    assert!(ok, "a dry-run promotion must exit 0; got:\n{out}");
    assert!(
        out.contains("would promote snapcraft alpha")
            && out.contains("would promote snapcraft beta"),
        "both workspace members' snaps must be promoted; got:\n{out}"
    );
}

/// A malformed channel is refused before any store call, and the verb folds it
/// into the aggregated per-publisher failure with a non-zero exit.
#[test]
fn promote_invalid_channel_fails_before_any_publisher_runs() {
    let dir = project(SINGLE_CRATE);
    let (ok, out) = promote(
        dir.path(),
        &["--to", "lastest", "--publishers", "snapcraft", "--dry-run"],
    );
    assert!(!ok, "a malformed channel must exit non-zero; got:\n{out}");
    assert!(
        out.contains("invalid snapcraft channel 'lastest'"),
        "the operator must see which channel was rejected; got:\n{out}"
    );
    assert!(
        out.contains("1 publisher failed to promote: snapcraft"),
        "the verb must aggregate the failure by publisher; got:\n{out}"
    );
}

/// A malformed `SOURCE_DATE_EPOCH` is the same error in `promote` as in a
/// release, and a valid one lets the promotion run.
#[test]
fn promote_refuses_a_malformed_source_date_and_runs_with_a_valid_one() {
    let dir = project(SINGLE_CRATE);
    let args = ["--to", "stable", "--dry-run"];
    let (ok, out) = promote_with_source_date(dir.path(), "soon", &args);
    assert!(
        !ok,
        "a malformed source date must exit non-zero; got:\n{out}"
    );
    assert!(
        out.contains("SOURCE_DATE_EPOCH is set to \"soon\""),
        "the error must name the variable and its value; got:\n{out}"
    );
    let (ok, out) = promote_with_source_date(dir.path(), "1715000000", &args);
    assert!(
        ok,
        "a valid source date must not stop the promotion; got:\n{out}"
    );
}
