//! Integration tests for the in-process failure policy
//! (`release.on_failure`).
//!
//! Each test drives the real binary against a throwaway git repo with a
//! local bare `origin`, fails the publish via the env-gated
//! `--simulate-failure` harness, and asserts the git-level outcome:
//!
//! - default `rollback` on a reversible-only failure reverts the bump
//!   commit and deletes the release tag (locally and on origin);
//! - `on_failure: hold` leaves the tag and commit in place;
//! - `rollback` auto-degrades to hold when a one-way-door publisher
//!   already landed (evidence planted as a per-crate run summary,
//!   the shape a prior crate's publish leaves in workspace per-crate
//!   mode), and the output names the burned publisher.
//!
//! Every test also asserts the run summary records the taken path in
//! its `failure_policy` field.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{run_git, tool_on_path};

use anodizer_core::test_helpers::{create_config, create_test_project};

/// Stage-skip list leaving only the publish stage live — mirrors the
/// existing required-publisher gate tests in `integration.rs`.
const SKIP_ALL_BUT_PUBLISH: &str = "--skip=build,upx,appbundle,dmg,msi,pkg,nsis,notarize,changelog,archive,source,nfpm,srpm,makeself,snapcraft,flatpak,sbom,templatefiles,checksum,sign,release,docker,docker-sign,blob,snapcraft-publish,announce";

/// Build a repo whose HEAD is a bump-style commit tagged `v0.1.0`, with
/// a local bare `origin` holding the branch and the tag — the state an
/// `anodizer tag --push` run leaves behind right before `release` runs.
fn setup_tagged_repo_with_origin(config: &str) -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    let origin = TempDir::new().unwrap();
    create_test_project(repo.path());
    create_config(repo.path(), config);

    run_git(repo.path(), &["init", "-q", "-b", "master"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["config", "commit.gpgsign", "false"]);
    run_git(repo.path(), &["add", "-A"]);
    run_git(repo.path(), &["commit", "-q", "-m", "initial"]);
    fs::write(repo.path().join("RELEASE.md"), "v0.1.0\n").unwrap();
    run_git(repo.path(), &["add", "-A"]);
    run_git(
        repo.path(),
        &["commit", "-q", "-m", "chore(release): v0.1.0"],
    );
    run_git(repo.path(), &["tag", "v0.1.0"]);

    run_git(origin.path(), &["init", "-q", "--bare", "-b", "master"]);
    let origin_path = origin.path().to_string_lossy().to_string();
    run_git(repo.path(), &["remote", "add", "origin", &origin_path]);
    run_git(repo.path(), &["push", "-q", "origin", "master", "v0.1.0"]);

    (repo, origin)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(dir);
            cmd
        },
        "git",
    );
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_failing_release(repo: &Path, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_anodizer"));
    cmd.args([
        "release",
        "--no-preflight",
        "--simulate-failure",
        "cargo",
        SKIP_ALL_BUT_PUBLISH,
    ])
    .env("ANODIZE_TEST_HARNESS", "1")
    .env_remove("CARGO_REGISTRY_TOKEN")
    .env_remove("GITHUB_TOKEN")
    .env_remove("GH_TOKEN")
    .env_remove("ANODIZER_GITHUB_TOKEN")
    .current_dir(repo);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.output().expect("invoking anodizer release")
}

fn summary_failure_policy(repo: &Path) -> serde_json::Value {
    let path = repo.join("dist").join("run-v0.1.0").join("summary.json");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("summary must exist at {}: {e}", path.display()));
    let summary: serde_json::Value = serde_json::from_str(&raw).expect("summary parses");
    summary
        .get("failure_policy")
        .unwrap_or_else(|| panic!("summary must record failure_policy: {raw}"))
        .clone()
}

const CARGO_PUBLISH_CONFIG: &str = r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#;

/// (a) Default policy: a reversible-only failure (required cargo
/// publisher failed — nothing landed) rolls back in-process: revert
/// commit created and pushed, tag deleted locally and on origin.
#[test]
fn release_failure_default_rollback_reverts_bump_and_deletes_tag() {
    if !tool_on_path("git") {
        eprintln!("SKIP release_failure_default_rollback: git missing");
        return;
    }
    let (repo, origin) = setup_tagged_repo_with_origin(CARGO_PUBLISH_CONFIG);
    let bump_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);

    // Hermetic crates.io index for the rollback path's GLOBAL published-state
    // probe: `test-project@0.1.0` is a real (unrelated) crate on the live
    // index, and the guard must consult registry state before deleting the
    // tag. A local 404 responder answers "version absent" so the test both
    // stays offline-safe and proves the probe actually ran end-to-end.
    let (index_addr, index_hits) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
    let index_base = format!("http://{index_addr}");
    let output = run_failing_release(
        repo.path(),
        &[("ANODIZER_TEST_CRATES_IO_INDEX_BASE", index_base.as_str())],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        index_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "rollback must consult the crates.io index for test-project@0.1.0 exactly once; \
         stderr: {stderr}"
    );
    assert!(
        !output.status.success(),
        "release must still exit non-zero after rollback; stderr: {stderr}"
    );
    assert!(
        stderr.contains("required publisher") && stderr.contains("cargo"),
        "original failure must not be masked by the policy; got: {stderr}"
    );
    assert!(
        stderr.contains("on_failure=rollback"),
        "policy must announce itself; got: {stderr}"
    );

    // Tag gone locally and on origin.
    assert_eq!(
        git_stdout(repo.path(), &["tag", "-l", "v0.1.0"]),
        "",
        "local tag must be deleted; stderr: {stderr}"
    );
    assert_eq!(
        git_stdout(origin.path(), &["tag", "-l", "v0.1.0"]),
        "",
        "remote tag must be deleted; stderr: {stderr}"
    );

    // HEAD is the revert of the bump commit, and it reached origin.
    let head_subject = git_stdout(repo.path(), &["log", "-1", "--format=%s"]);
    assert!(
        head_subject.contains("rollback v0.1.0"),
        "HEAD must be the rollback revert commit, got: {head_subject}"
    );
    let head_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(head_sha, bump_sha, "revert must advance HEAD");
    assert_eq!(
        git_stdout(origin.path(), &["rev-parse", "refs/heads/master"]),
        head_sha,
        "revert commit must be pushed to origin"
    );

    let record = summary_failure_policy(repo.path());
    assert_eq!(record["configured"], "rollback");
    assert_eq!(record["action"], "rolled-back");
    assert_eq!(record["degraded"], false);
}

/// (b) `on_failure: hold` leaves every piece of state in place: tag
/// intact (local + origin), no revert commit, summary records the hold.
#[test]
fn release_failure_hold_leaves_state_in_place() {
    if !tool_on_path("git") {
        eprintln!("SKIP release_failure_hold: git missing");
        return;
    }
    let config = r#"
project_name: test-project
release:
  on_failure: hold
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#;
    let (repo, origin) = setup_tagged_repo_with_origin(config);
    let bump_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);

    let output = run_failing_release(repo.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "hold must still exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("on_failure=hold") && stderr.contains("--rollback-only"),
        "hold must say so and point at the manual recovery path; got: {stderr}"
    );

    assert_eq!(git_stdout(repo.path(), &["tag", "-l", "v0.1.0"]), "v0.1.0");
    assert_eq!(
        git_stdout(origin.path(), &["tag", "-l", "v0.1.0"]),
        "v0.1.0"
    );
    assert_eq!(
        git_stdout(repo.path(), &["rev-parse", "HEAD"]),
        bump_sha,
        "no revert commit may be created under hold"
    );

    let record = summary_failure_policy(repo.path());
    assert_eq!(record["configured"], "hold");
    assert_eq!(record["action"], "held");
    assert_eq!(record["degraded"], false);
}

/// Lay down a preserved dist tree the way `check determinism
/// --preserve-dist` would (archive + artifacts.json + context.json),
/// minimal enough for `--publish-only` to load. Mirrors the fixture in
/// `publish_only.rs`.
fn bootstrap_preserved_dist(repo: &Path, crate_name: &str, version: &str, commit: &str) {
    let dist = repo.join("dist");
    fs::create_dir_all(&dist).unwrap();
    let target = "x86_64-unknown-linux-gnu";
    let archive_name = format!("{crate_name}_{version}_{target}.tar.gz");
    let archive_path = dist.join(&archive_name);
    fs::write(&archive_path, b"ARCHIVE\n").unwrap();

    let artifacts_json = serde_json::json!([
        {
            "kind": "archive",
            "name": archive_name,
            "path": archive_path.to_string_lossy(),
            "target": target,
            "crate_name": crate_name,
            "metadata": { "ID": crate_name, "Format": "tar.gz" },
        }
    ]);
    fs::write(
        dist.join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json).unwrap(),
    )
    .unwrap();

    // sha256 of the literal b"ARCHIVE\n" payload above — publish-only
    // hash-verifies every preserved artifact before running.
    let archive_sha256 = "5c1f30e5f037be631bf54f0b521304c77bb439bfe90f7839b885a1b5099c724c";
    let context_json = serde_json::json!({
        "artifacts": [
            {
                "name": archive_name,
                "path": archive_name,
                "sha256": format!("sha256:{archive_sha256}"),
                "size": 8u64,
            }
        ],
        "targets": [target],
        "version": version,
        "commit": commit,
    });
    fs::write(
        dist.join("context.json"),
        serde_json::to_string_pretty(&context_json).unwrap(),
    )
    .unwrap();
}

/// (c) Auto-degrade: with `on_failure: rollback` (default) but a
/// one-way-door publisher already landed — evidence persisted as a
/// per-crate run summary, exactly what a prior crate's publish leaves
/// behind in workspace per-crate mode — the policy must NOT roll back:
/// tag and commit stay, the output says it degraded and names the
/// burned publisher, and the summary records the degrade. Runs in
/// `--publish-only` mode (the preserved-dist consumer), which is also
/// the mode anodizer's own release pipeline uses.
#[test]
fn publish_only_failure_degrades_rollback_to_hold_after_one_way_door() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_failure_degrades: git missing");
        return;
    }
    const CRATE: &str = "anodize-failure-policy-fixture";
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    common::bootstrap_minimal_cargo_repo(repo, CRATE);
    let host = common::host_triple();
    let yaml = format!(
        r#"project_name: {CRATE}
crates:
  - name: {CRATE}
    path: .
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - id: {CRATE}
        binary: {CRATE}
        targets:
          - {host}
    publish:
      cargo: {{}}
"#
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    fs::write(repo.join(".gitignore"), "dist/\n").unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "configure release fixture"]);
    let bump_sha = git_stdout(repo, &["rev-parse", "HEAD"]);
    run_git(repo, &["tag", "-a", "v0.1.0", "-m", "release v0.1.0"]);

    bootstrap_preserved_dist(repo, CRATE, "0.1.0", &bump_sha);

    // A sibling crate's persisted summary: cargo landed for it earlier
    // in this run — the version is burned.
    let burned = serde_json::json!({
        "schema_version": 1,
        "anodize_version": "0.0.0-test",
        "tag": "sibling-v0.1.0",
        "submitter_gated": false,
        "announce_gated": false,
        "publishers_succeeded": 1,
        "publishers_failed": 0,
        "irreversibly_published": true,
        "results": [
            {
                "name": "cargo",
                "group": "Submitter",
                "required": true,
                "status": "succeeded",
                "evidence": null,
            }
        ],
        "determinism_allowlist": { "compile_time": [], "runtime": [] },
    });
    let planted = repo
        .join("dist")
        .join("sibling")
        .join("run-sibling-v0.1.0")
        .join("summary.json");
    fs::create_dir_all(planted.parent().unwrap()).unwrap();
    fs::write(&planted, serde_json::to_string_pretty(&burned).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publish-only",
            "--no-preflight",
            "--simulate-failure",
            "cargo",
            SKIP_ALL_BUT_PUBLISH,
        ])
        .env("ANODIZE_TEST_HARNESS", "1")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodizer release --publish-only");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "degraded run must exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("DEGRADED") && stderr.contains("cargo"),
        "degrade must be loud and name the burned publisher; got: {stderr}"
    );

    // Nothing destructive happened.
    assert_eq!(git_stdout(repo, &["tag", "-l", "v0.1.0"]), "v0.1.0");
    assert_eq!(
        git_stdout(repo, &["rev-parse", "HEAD"]),
        bump_sha,
        "degraded rollback must not create a revert commit"
    );

    let record = summary_failure_policy(repo);
    assert_eq!(record["configured"], "rollback");
    assert_eq!(record["action"], "held");
    assert_eq!(record["degraded"], true);
    assert_eq!(record["burned_publishers"], serde_json::json!(["cargo"]));

    // The degrade record reaches the planted per-crate summary too —
    // both layout levels carry the audit trail.
    let sibling: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&planted).unwrap()).unwrap();
    assert_eq!(sibling["failure_policy"]["action"], "held");
}

/// The publish-only credential gate scopes its production-signing-key
/// requirement to the RESOLVED publisher surface. A signing-free surface
/// — here cargo-only, with `--skip` deselecting the `sign` stage and
/// every `signs:` consumer (github-release / blob / artifactory /
/// uploads) — needs no cosign/GPG material, so a run with a release token
/// but NO signing key must PASS the credential gate and proceed into the
/// publish pipeline. This is the regression behind the github-hosted
/// npm-provenance job (`--publish-only --publishers npm`, which carries
/// no signing keys by design): the old blanket gate aborted it with
/// "missing production signing key" before it could publish.
#[test]
fn publish_only_signing_free_surface_passes_credential_gate() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_signing_free_surface: git missing");
        return;
    }
    let (repo, origin) = setup_tagged_repo_with_origin(CARGO_PUBLISH_CONFIG);
    let bump_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    // The preserved dist is laid down BEFORE the run here (unlike tests
    // (a)/(b), where the run itself creates it after the dirty-repo
    // check), so it must be ignored or setup aborts on a dirty tree.
    // Self-ignoring keeps the untracked .gitignore out of the way too.
    fs::write(repo.path().join(".gitignore"), "dist/\n.gitignore\n").unwrap();
    bootstrap_preserved_dist(repo.path(), "test-project", "0.1.0", &bump_sha);

    // The publish-only path now runs the live publisher preflight (a mode that
    // crosses one-way doors must probe registry state). Keep it hermetic: a
    // local 200 answers the crates.io token-validity probe (a dummy token would
    // otherwise 401 against live crates.io and false-block this credential-gate
    // test), and a 404 answers the already-published index probe as "absent".
    let (api_addr, _) = anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
    ]);
    let (index_addr, _) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", SKIP_ALL_BUT_PUBLISH])
        .env("GITHUB_TOKEN", "dummy-token")
        .env("CARGO_REGISTRY_TOKEN", "dummy-token")
        .env("ANODIZE_TEST_HARNESS", "1")
        .env(
            "ANODIZER_TEST_CRATES_IO_API_BASE",
            format!("http://{api_addr}"),
        )
        .env(
            "ANODIZER_TEST_CRATES_IO_INDEX_BASE",
            format!("http://{index_addr}"),
        )
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .current_dir(repo.path())
        .output()
        .expect("invoking anodizer release --publish-only");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The credential gate must NOT trip on the missing signing key for a
    // surface that consumes no production signatures.
    assert!(
        !stderr.contains("missing production signing key"),
        "a signing-free surface must not be blocked on the signing key; got: {stderr}"
    );
    // The run reached the publish pipeline (proof it got past the gate):
    // the cargo publisher's own log line appears.
    assert!(
        stderr.contains("cargo publish"),
        "run must proceed past the credential gate into the publish pipeline; got: {stderr}"
    );
    // The original tags stay intact; nothing destructive ran.
    assert_eq!(git_stdout(repo.path(), &["tag", "-l", "v0.1.0"]), "v0.1.0");
    assert_eq!(
        git_stdout(origin.path(), &["tag", "-l", "v0.1.0"]),
        "v0.1.0"
    );
}

/// Counterpart to the signing-free case: a publish-only surface that DOES
/// keep a `signs:` consumer selected (github-release) must STILL abort on the
/// signing requirement — the guard is scoped, not removed. Zero-mutation: no
/// rollback, no revert commit, tags intact on both ends, no `failure_policy`
/// record written. The scoping itself (the sign stage's `signs:` loop
/// self-skips for a signing-free surface so no `KeyEnv` requirement is
/// collected, but stands otherwise) is unit-tested via `collect_requirements`
/// in `preflight.rs`; this case proves the end-to-end abort.
#[test]
fn publish_only_signing_surface_still_requires_sign_key() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_signing_surface_requires_key: git missing");
        return;
    }
    // A surface with a `signs:` slice and a selected signs consumer
    // (github-release). The `--skip` list below deselects everything EXCEPT
    // sign + release, so `signs_fully_deselected` is false and signing is
    // genuinely required.
    let signing_config = r#"
project_name: test-project
signs:
  - artifacts: all
    cmd: cosign
    args: ["sign-blob", "--key=env://COSIGN_KEY", "--output-signature=${signature}", "${artifact}"]
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: test-owner
        name: test-repo
"#;
    let (repo, origin) = setup_tagged_repo_with_origin(signing_config);
    let bump_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join(".gitignore"), "dist/\n.gitignore\n").unwrap();
    bootstrap_preserved_dist(repo.path(), "test-project", "0.1.0", &bump_sha);

    // Keep sign + release (the signs consumer) selected; skip the rest.
    let skip_all_but_sign_release = "--skip=build,upx,appbundle,dmg,msi,pkg,nsis,notarize,changelog,archive,source,nfpm,\
         srpm,makeself,snapcraft,flatpak,sbom,templatefiles,checksum,docker,docker-sign,blob,\
         snapcraft-publish,announce,cargo";

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", skip_all_but_sign_release])
        .env("GITHUB_TOKEN", "dummy-token")
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .current_dir(repo.path())
        .output()
        .expect("invoking anodizer release --publish-only");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "missing sign key must abort a signing surface; stderr: {stderr}"
    );
    // The config-derived env preflight is the single front line: the sign
    // stage's `signs:` loop is live (a signs consumer is selected), so it
    // contributes a `KeyEnv` requirement for the missing cosign key, and the
    // report attributes the failure to `stage:sign`. This proves a signing
    // surface cannot silently proceed without signing material.
    assert!(
        stderr.contains("stage:sign"),
        "a signing surface must abort naming the signing requirement; got: {stderr}"
    );
    assert!(
        !stderr.contains("on_failure") && !stderr.contains("rolling back"),
        "a zero-mutation credential miss must never reach the failure policy; got: {stderr}"
    );

    // Nothing was touched: tag intact on both ends, HEAD unchanged.
    assert_eq!(git_stdout(repo.path(), &["tag", "-l", "v0.1.0"]), "v0.1.0");
    assert_eq!(
        git_stdout(origin.path(), &["tag", "-l", "v0.1.0"]),
        "v0.1.0"
    );
    assert_eq!(
        git_stdout(repo.path(), &["rev-parse", "HEAD"]),
        bump_sha,
        "no revert commit may be created; stderr: {stderr}"
    );

    // No run summary / failure_policy record — the run never reached
    // the publish pipeline, let alone the policy.
    assert!(
        !repo
            .path()
            .join("dist")
            .join("run-v0.1.0")
            .join("summary.json")
            .exists(),
        "credential-gate abort must not write a failure-policy summary"
    );
}

/// `release.on_failure` is root-level only: a crate-level setting is a
/// config-load error (never silently ignored), surfaced by every
/// config-loading command.
#[test]
fn crate_level_on_failure_is_rejected_at_config_load() {
    if !tool_on_path("git") {
        eprintln!("SKIP crate_level_on_failure_rejected: git missing");
        return;
    }
    let config = r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      on_failure: hold
    publish:
      cargo: {}
"#;
    let (repo, _origin) = setup_tagged_repo_with_origin(config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(repo.path())
        .output()
        .expect("invoking anodizer check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "crate-level on_failure must fail config load; stderr: {stderr}"
    );
    assert!(
        stderr.contains("root-level") && stderr.contains("test-project"),
        "error must explain the rule and name the crate; got: {stderr}"
    );
}
