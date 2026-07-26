//! Root `always:` hooks — the `finally` of `release` and `build` — on the
//! paths a post-run hook can silently never reach, plus the lane ordering.
//!
//! Each test drives the real binary against a fixture whose `after:`,
//! `on_error:` and `always:` blocks all append their own label to one
//! marker file, so both "which lanes fired" and "in what order" are proven
//! by side effect rather than by a log line.
//!
//! Shell hooks run through `sh -c`, so the whole file is Unix-only.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use anodizer_core::test_helpers::{create_config, create_test_project, init_git_repo};

/// Config whose `after:` / `on_error:` / `always:` blocks each append one
/// labelled line to `marker`. `before`, when set, becomes the root
/// `before:` hook command.
fn config_with_hook_lanes(marker: &Path, before: Option<&str>) -> String {
    let before_block = match before {
        Some(cmd) => format!("before:\n  hooks:\n    - '{cmd}'\n"),
        None => String::new(),
    };
    let marker = marker.display();
    let host = anodizer_cli::detect_host_target().expect("rustc -vV must succeed in test env");
    format!(
        r#"project_name: always-fixture
{before_block}after:
  hooks:
    - 'printf "after\n" >> {marker}'
on_error:
  hooks:
    - 'printf "on_error\n" >> {marker}'
always:
  hooks:
    - 'printf "always success=$ANODIZER_SUCCESS\n" >> {marker}'
crates:
  - name: always-fixture
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: always-fixture
        targets:
          - {host}
"#,
    )
}

/// Fixture repo whose committed tree already contains the config, so the
/// dirty-tree gate passes on the modes that enforce it (`--split`,
/// `--merge`).
fn setup_fixture(tmp: &Path, marker: &Path, before: Option<&str>) {
    create_test_project(tmp);
    create_config(tmp, &config_with_hook_lanes(marker, before));
    init_git_repo(tmp);
}

fn run_anodizer(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(args)
        .current_dir(tmp)
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .output()
        .expect("invoke anodizer")
}

/// Heavy artifact stages carry no signal for a hook-ordering assertion;
/// skipping them keeps every test in this file cheap on any host.
const SKIP_HEAVY: &str = "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom,upx";

/// The labels the hook lanes appended, in the order they fired.
fn lanes_fired(marker: &Path, out: &std::process::Output, label: &str) -> Vec<String> {
    let body = fs::read_to_string(marker).unwrap_or_else(|e| {
        panic!(
            "{label}: expected hook lanes to have fired ({}): {e}\nstderr:\n{}",
            marker.display(),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    body.lines().map(|l| l.trim().to_string()).collect()
}

/// A `before:` hook that fails aborts the run before the pipeline starts —
/// the exit `after:` structurally cannot reach and `on_error:` (scoped to a
/// dispatched-mode failure) deliberately does not cover. `always:` must
/// still fire, and must see the run as failed.
#[test]
fn always_hooks_fire_when_before_hooks_fail() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    setup_fixture(tmp.path(), &marker, Some("exit 7"));

    let out = run_anodizer(
        tmp.path(),
        &["release", "--snapshot", SKIP_HEAVY, "--timeout", "2m"],
    );

    assert!(
        !out.status.success(),
        "a failing before: hook must fail the run.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lanes = lanes_fired(&marker, &out, "before-hook failure");
    assert_eq!(
        lanes,
        vec!["always success=false"],
        "only always: reaches a before-hook failure, and it must see success=false"
    );
}

/// The `--split` build leg runs the build stage and returns — it never
/// reaches the post-pipeline tail where `after:` fires. A teardown hook
/// wired next to `after:` would silently never run on a shard, so
/// `always:` must fire from the command's own exit instead.
#[test]
fn always_hooks_fire_on_the_split_build_leg() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    setup_fixture(tmp.path(), &marker, None);

    let out = run_anodizer(
        tmp.path(),
        &["release", "--split", SKIP_HEAVY, "--timeout", "2m"],
    );

    let lanes = lanes_fired(&marker, &out, "split build leg");
    assert_eq!(
        lanes,
        vec!["always success=true"],
        "the shard leg never reaches after:, so always: is the only lane that \
         can clean up after it.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The `--merge` leg is a separate process invocation that runs its own
/// `before:` hooks, so it must run its own `always:` hooks too. Driven
/// against an empty dist: the merge fails for lack of split contexts, which
/// is exactly the case a teardown hook exists for — and pins the failure
/// ordering, `on_error:` first and `always:` last.
#[test]
fn always_hooks_fire_last_on_the_split_merge_leg() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    setup_fixture(tmp.path(), &marker, None);
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--merge",
            "--no-env-preflight",
            SKIP_HEAVY,
            "--timeout",
            "2m",
        ],
    );

    let lanes = lanes_fired(&marker, &out, "split merge leg");
    assert_eq!(
        lanes,
        vec!["on_error", "always success=false"],
        "the merge leg must fire on_error: then always:, in that order.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Success ordering: `after:` runs first, `always:` last. Asserted through
/// the dry-run previews, which name each lane in the order the run reaches
/// it without executing anything.
#[test]
fn always_hooks_run_after_the_after_hooks_on_success() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    setup_fixture(tmp.path(), &marker, None);

    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--snapshot",
            "--dry-run",
            SKIP_HEAVY,
            "--timeout",
            "2m",
        ],
    );
    assert!(
        out.status.success(),
        "the dry-run snapshot must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    let after = stderr
        .find("would run after hook")
        .unwrap_or_else(|| panic!("after: hook must be previewed.\nstderr:\n{stderr}"));
    let always = stderr
        .find("would run always hook")
        .unwrap_or_else(|| panic!("always: hook must be previewed.\nstderr:\n{stderr}"));
    assert!(
        after < always,
        "always: must run after after: on the success path.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("would run on-error hook"),
        "on_error: must not fire on a successful run.\nstderr:\n{stderr}"
    );
}

/// Config for the `build`-command bracket, named after the
/// `create_test_project` package so the build stage compiles a real
/// binary. Carries all four root lanes so the assertions pin which ones
/// `anodizer build` reaches AND which it does not.
fn build_config_with_hook_lanes(marker: &Path, before: &str) -> String {
    let marker = marker.display();
    let host = anodizer_cli::detect_host_target().expect("rustc -vV must succeed in test env");
    format!(
        r#"project_name: test-project
before:
  hooks:
    - '{before}'
after:
  hooks:
    - 'printf "after\n" >> {marker}'
on_error:
  hooks:
    - 'printf "on_error\n" >> {marker}'
always:
  hooks:
    - 'printf "always success=$ANODIZER_SUCCESS\n" >> {marker}'
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
"#,
    )
}

/// `anodizer build` runs root `before:` hooks, so state staged there needs
/// a teardown lane on the same command. The bracket is the full
/// `before` → work → `after` → `always`, in that order, on a build that
/// succeeded.
///
/// `on_error:` is deliberately absent from `build`'s lane set: it is the
/// release-failed notification lane, and a local build failure is not a
/// failed release. The fixture configures it anyway so this asserts the
/// scoping instead of leaving it undocumented.
#[test]
fn build_command_fires_the_whole_root_bracket_on_success() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        &build_config_with_hook_lanes(
            &marker,
            &format!("printf \"before\\n\" >> {}", { marker.display() }),
        ),
    );
    init_git_repo(tmp.path());

    let out = run_anodizer(tmp.path(), &["build", "--timeout", "5m"]);
    assert!(
        out.status.success(),
        "anodizer build must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Vacuity guard: the bracket must close around a build that actually
    // produced something, not around a no-op that reached the success path
    // without compiling anything.
    let artifacts = fs::read_to_string(tmp.path().join("dist/artifacts.json"))
        .expect("the build must have written dist/artifacts.json");
    assert!(
        artifacts.contains("\"binary\""),
        "the build must have produced a binary artifact: {artifacts}"
    );

    let lanes = lanes_fired(&marker, &out, "build success");
    assert_eq!(
        lanes,
        vec!["before", "after", "always success=true"],
        "build must close the bracket it opens: after: then always:, both \
         after the before: hooks.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A `before:` hook that fails aborts `anodizer build` before the build
/// stage — the exit `after:` structurally cannot reach. `always:` must
/// still fire and see the run as failed, which is the whole point of
/// giving `build` a teardown lane: whatever `before:` staged gets cleaned
/// up even though the run never got anywhere.
#[test]
fn build_command_fires_always_when_before_hooks_fail() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("lanes.txt");
    create_test_project(tmp.path());
    create_config(tmp.path(), &build_config_with_hook_lanes(&marker, "exit 7"));
    init_git_repo(tmp.path());

    let out = run_anodizer(tmp.path(), &["build", "--timeout", "5m"]);
    assert!(
        !out.status.success(),
        "a failing before: hook must fail the build.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lanes = lanes_fired(&marker, &out, "build before-hook failure");
    assert_eq!(
        lanes,
        vec!["always success=false"],
        "only always: reaches a before-hook failure, and it must see \
         success=false"
    );
}
