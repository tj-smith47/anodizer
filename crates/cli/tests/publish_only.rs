//! Integration tests for `anodize release --publish-only`.
//!
//! - **Dry-run** — `--publish-only` loads context.json + artifacts.json
//!   from a pre-populated dist, then runs the publish pipeline
//!   (`SignStage` head + `ReleaseStage` + `PublishStage` + ...) in
//!   dry-run mode. Asserts the publish pipeline starts and that the
//!   build/archive/nfpm stages are NOT exercised (no recompile).
//!
//! - **Re-sign** — provides a production cosign/GPG keypair via env,
//!   runs `--publish-only`, and verifies the resulting signatures
//!   against the production public key. `#[ignore]`d by default because
//!   cosign + gpg aren't reliably available in every CI shard; an opt-
//!   in run validates the idempotence + ephemeral-strip path end-to-
//!   end.
//!
//! All tests skip cleanly on hosts without the required tools (cargo,
//! git) — same convention as `preserve_dist.rs` / `check_determinism.rs`.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, run_git, tool_on_path};

const FIXTURE_CRATE_NAME: &str = "anodize-publish-only-fixture";

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Synthesize a `dist/` whose contents look like what
/// `anodize check determinism --preserve-dist=<dist>` would have
/// written: at least one archive + sidecar `artifacts.json` +
/// `metadata.json` + `context.json` + a sha256 sidecar.
///
/// The shapes mirror what the determinism harness preserves on a green
/// run:
///   - `<dist>/<crate>_<version>_<os>_<arch>.tar.gz` — fake archive
///     bytes (a sentinel `b"ARCHIVE\n"` payload). Real archives in
///     production are produced by `stage-archive`; for `--dry-run` the
///     bytes only need to exist so signature-target paths resolve.
///   - `<dist>/artifacts.json` — the in-process registry shape
///     `load_artifacts_from_dist` parses (kind / path / target /
///     crate_name).
///   - `<dist>/context.json` — the harness's PreservedDistContext
///     shape (artifacts + targets + version + commit).
///   - `<dist>/metadata.json` — `{ project_name, tag, version, commit }`,
///     primarily for harness round-trips. Not required by the
///     publish-only loader but present in production preserved trees.
fn bootstrap_preserved_dist(
    repo: &Path,
    version: &str,
    commit_in_context: &str,
) -> (String, String) {
    let dist = repo.join("dist");
    fs::create_dir_all(&dist).unwrap();

    let target = "x86_64-unknown-linux-gnu";
    let archive_name = format!("{FIXTURE_CRATE_NAME}_{version}_{target}.tar.gz");
    let archive_path = dist.join(&archive_name);
    fs::write(&archive_path, b"ARCHIVE\n").unwrap();

    // artifacts.json — what stage-publish's load_artifacts_from_dist reads.
    let artifacts_json = serde_json::json!([
        {
            "kind": "archive",
            "name": archive_name,
            "path": archive_path.to_string_lossy(),
            "target": target,
            "crate_name": FIXTURE_CRATE_NAME,
            "metadata": {
                "ID": FIXTURE_CRATE_NAME,
                "Format": "tar.gz",
            },
        }
    ]);
    fs::write(
        dist.join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json).unwrap(),
    )
    .unwrap();

    // context.json — the Phase-1 harness's PreservedDistContext shape.
    // sha256 must match the on-disk bytes of `archive_path` because
    // publish-only hash-verifies every preserved artifact before
    // re-signing. Pinned literal is the sha256 of `b"ARCHIVE\n"`.
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
        "commit": commit_in_context,
    });
    fs::write(
        dist.join("context.json"),
        serde_json::to_string_pretty(&context_json).unwrap(),
    )
    .unwrap();

    // metadata.json — what the post-pipeline writes; not load-bearing
    // here but matches the production preserved-dist shape so the
    // fixture stays close to reality.
    let metadata_json = serde_json::json!({
        "project_name": FIXTURE_CRATE_NAME,
        "tag": format!("v{version}"),
        "version": version,
        "commit": commit_in_context,
    });
    fs::write(
        dist.join("metadata.json"),
        serde_json::to_string_pretty(&metadata_json).unwrap(),
    )
    .unwrap();

    (archive_name, target.to_string())
}

/// Bytes the current binary would never reproduce for `dist/config.yaml`
/// (it serializes the effective `Config`, never this literal). A
/// publish-only run that left these bytes untouched proves it did not
/// re-render config.yaml; a run that overwrote them changes the sha256
/// and trips hash-verify.
const SENTINEL_CONFIG_YAML: &[u8] =
    b"# SENTINEL preserved config.yaml \xe2\x80\x94 must survive publish-only untouched\nproject_name: anodize-publish-only-fixture\n__sentinel__: do-not-regenerate\n";

/// Drop a sentinel `dist/config.yaml` into the preserved tree and append
/// its `config.yaml` entry (with the matching sha256) to the existing
/// `context.json`, so `hash_verify_preserved_dist` covers it. Mirrors
/// production: the determinism harness records `dist/config.yaml`'s hash
/// across shards, and publish-only verifies the on-disk bytes against it.
fn inject_preserved_config_yaml(repo: &Path) -> Vec<u8> {
    let dist = repo.join("dist");
    let config_path = dist.join("config.yaml");
    fs::write(&config_path, SENTINEL_CONFIG_YAML).unwrap();

    // sha256 of SENTINEL_CONFIG_YAML (pinned literal; recomputing in-test
    // would mask a divergence between the bytes and the recorded hash).
    let config_sha256 = "76874b4862b3be0bfcb23289f4c6dc68a0d425e5f687207ce9eedb58a1b82278";

    let context_path = dist.join("context.json");
    let raw = fs::read_to_string(&context_path).unwrap();
    let mut context: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let artifacts = context
        .get_mut("artifacts")
        .and_then(|a| a.as_array_mut())
        .expect("context.json artifacts array");
    artifacts.push(serde_json::json!({
        "name": "config.yaml",
        "path": "config.yaml",
        "sha256": format!("sha256:{config_sha256}"),
        "size": SENTINEL_CONFIG_YAML.len() as u64,
    }));
    fs::write(
        &context_path,
        serde_json::to_string_pretty(&context).unwrap(),
    )
    .unwrap();

    SENTINEL_CONFIG_YAML.to_vec()
}

/// Rewrite `.anodizer.yaml` with a `tag_template: "v{{ .Version }}"`
/// entry. The minimal fixture from `bootstrap_minimal_cargo_repo`
/// omits the template, which leaves it as the empty-string default
/// (`anodizer_core::config::CrateConfig::default`); the empty template
/// makes `find_latest_tag_matching_with_prefix` produce an `^$` regex
/// that never matches any tag, and `--publish-only` (non-dry-run)
/// bails on "no git tag found" before reaching the publish-only
/// branch.
///
/// Commits the rewrite so `git describe` sees a tag pointing at HEAD.
fn configure_tag_template(repo: &Path) {
    let host = common::host_triple();
    // Include a `release:` block (with `disable_upload: true` so the
    // dry-run release stage still emits its "would create GitHub
    // Release ... tag=..." line in stdout — that's the substring
    // the pipeline-composition test asserts against).
    let yaml = format!(
        r#"project_name: {crate_name}
crates:
  - name: {crate_name}
    path: .
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - id: {crate_name}
        binary: {crate_name}
        targets:
          - {host}
    release:
      github:
        owner: test-owner
        name: test-repo
"#,
        crate_name = FIXTURE_CRATE_NAME,
        host = host,
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    // Gitignore dist/ so the preserved-dist files bootstrapped later
    // don't trip anodize's `git is in a dirty state` check
    // (release-resolver bails dirty unless --snapshot).
    fs::write(repo.join(".gitignore"), "dist/\n").unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "configure tag_template"]);
}

/// Resolve the fixture repo's HEAD commit SHA so the publish-only
/// commit cross-check (preserved.commit vs ctx.FullCommit) lines up.
fn head_commit(repo: &Path) -> String {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(repo).args(["rev-parse", "HEAD"]);
            cmd
        },
        "git",
    );
    assert!(out.status.success(), "git rev-parse failed: {:?}", out);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Tag the fixture repo at HEAD so `release` resolves a Git.Tag /
/// version triplet without needing `--snapshot`. Annotated tag —
/// `git describe` (which anodize's tag resolver uses) ignores
/// lightweight tags by default.
fn tag_head(repo: &Path, version: &str) -> String {
    let tag = format!("v{version}");
    run_git(repo, &["tag", "-a", &tag, "-m", &format!("release {tag}")]);
    tag
}

// ---------------------------------------------------------------------------
// Test 3: --publish-only dry-run consumes context.json
// ---------------------------------------------------------------------------

/// Spec Test 3: `--publish-only --dry-run` loads context.json +
/// artifacts.json, starts the publish pipeline (SignStage / ReleaseStage
/// / PublishStage emit log lines), and does NOT exercise the build /
/// archive / nfpm stages.
///
/// Drives `anodize release --publish-only --dry-run --no-preflight`
/// against a pre-bootstrapped dist. Asserts on stdout markers that pin
/// the pipeline composition end-to-end.
#[test]
fn publish_only_dry_run_consumes_context_json_and_runs_publish_pipeline() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_dry_run_consumes_context_json: git missing from PATH");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);

    let version = "0.1.0";
    let commit = head_commit(repo);
    let (archive_name, _target) = bootstrap_preserved_dist(repo, version, &commit);
    let tag = tag_head(repo, version);

    // The fixture's .anodizer.yaml from `configure_tag_template`
    // configures a single `builds:` entry but no archives / signs /
    // release sections. For `--publish-only --dry-run` that's fine —
    // SignStage runs a no-op when `signs:` is empty, ReleaseStage's
    // dry-run path doesn't require GitHub credentials, and the dispatch
    // we want to observe is the pipeline ORDERING, not the actual
    // upload. Asserting on the pipeline's stage-name banners is enough.
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publish-only",
            "--dry-run",
            "--no-preflight",
            "--skip",
            "announce", // no announce config in the fixture
        ])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "expected zero exit; stdout=\n{}\nstderr=\n{}",
        stdout,
        stderr
    );

    // The publish-only banner must fire.
    let merged = format!("{stdout}\n{stderr}");
    assert!(
        merged.contains("publish-only"),
        "expected publish-only mode banner in output; got:\n{merged}"
    );

    // The pipeline composition we want: sign + release + publish stage
    // names must appear in the stage-logger output (they emit a banner
    // each time they run). Build / archive / nfpm stages MUST NOT —
    // those produce artifacts and `--publish-only` is consuming them.
    //
    // The Pipeline runner emits a per-stage banner of the shape `name`
    // via StageLogger; relying on stage name substrings is the
    // load-bearing assertion. A future formatting change to that
    // banner should update this test alongside.
    for must_appear in &["sign", "release", "publish"] {
        assert!(
            merged.contains(must_appear),
            "expected stage `{must_appear}` to run in publish-only pipeline; \
             output was:\n{merged}"
        );
    }
    for must_not_appear in &["building binaries", "archiving", "building nfpm"] {
        assert!(
            !merged.contains(must_not_appear),
            "stage with marker `{must_not_appear}` ran but publish-only must not \
             trigger build/archive/nfpm; output was:\n{merged}"
        );
    }

    // The rehydrated artifact list must include the fixture archive (so
    // we know the loader picked up artifacts.json, not silently emptied
    // the registry).
    assert!(
        merged.contains(&archive_name) || merged.contains("rehydrated 1 artifact"),
        "expected artifact rehydration log line referencing `{archive_name}` \
         or `rehydrated 1 artifact`; output was:\n{merged}"
    );

    // Tag presence: release stage's dry-run path mentions the
    // resolved Git.Tag (e.g. "would create GitHub Release ... tag=v0.1.0").
    // A hard assertion pins that publish-only resolves the tag and
    // surfaces it in the dry-run summary — silent absence would mask
    // a regression where the tag resolver short-circuits before
    // ReleaseStage.
    assert!(
        merged.contains(&tag),
        "expected resolved tag {tag} in CLI output (release stage dry-run summary); \
         output was:\n{merged}"
    );
}

// ---------------------------------------------------------------------------
// Test 3a-bis: publish-only must NOT re-render the preserved config.yaml
// ---------------------------------------------------------------------------

/// Regression: `--publish-only` must leave the preserved
/// `dist/config.yaml` byte-identical. The release setup block calls
/// `write_effective_config` (which re-renders config.yaml from the
/// current binary's serialization); in publish-only that overwrite makes
/// the on-disk bytes diverge from the determinism-recorded sha256 and
/// `hash_verify_preserved_dist` aborts the publish. A binary newer than
/// the one that preserved the dist serializes config differently, so the
/// cross-version backfill workflow can never publish without the guard.
///
/// Stages a sentinel `dist/config.yaml` the current binary would never
/// reproduce, records its hash in `context.json`, runs the binary, and
/// asserts the bytes are unchanged AND the run does not fail hash-verify
/// on config.yaml.
#[test]
fn publish_only_does_not_overwrite_preserved_config_yaml() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_does_not_overwrite_preserved_config_yaml: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);

    let version = "0.1.0";
    let commit = head_commit(repo);
    bootstrap_preserved_dist(repo, version, &commit);
    let sentinel = inject_preserved_config_yaml(repo);
    tag_head(repo, version);

    let config_path = repo.join("dist/config.yaml");

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publish-only",
            "--dry-run",
            "--no-preflight",
            "--skip",
            "announce",
        ])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let merged = format!("{stdout}\n{stderr}");

    // (b) The run must not fail hash-verify on config.yaml.
    assert!(
        !merged.contains("config.yaml")
            || !merged.contains("hash-verify")
            || !merged.contains("diverge"),
        "publish-only must not trip hash-verify on the preserved config.yaml; output was:\n{merged}"
    );
    assert!(
        output.status.success(),
        "expected zero exit (preserved config.yaml left intact); stdout=\n{stdout}\nstderr=\n{stderr}"
    );

    // (a) The sentinel bytes must be unchanged after the run.
    let after = fs::read(&config_path).expect("preserved config.yaml must still exist");
    assert_eq!(
        after, sentinel,
        "publish-only overwrote the preserved dist/config.yaml; \
         write_effective_config must be skipped in publish-only mode"
    );
}

// ---------------------------------------------------------------------------
// Test 3b: missing context.json gives a clear error
// ---------------------------------------------------------------------------

/// Negative test: `--publish-only` against a `dist/` that lacks
/// `context.json` must surface a clear error pointing at the
/// `--preserve-dist` flag (so the operator knows where to look).
#[test]
fn publish_only_missing_context_json_errors_clearly() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_missing_context_json_errors_clearly: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    // Create dist/ with one file BUT no context.json — must trip the
    // loader, not the dist-non-empty pre-check.
    fs::create_dir_all(repo.join("dist")).unwrap();
    fs::write(repo.join("dist/placeholder"), b"x").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", "--dry-run", "--no-preflight"])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only");

    assert!(
        !output.status.success(),
        "expected non-zero exit when context.json is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("context.json") && stderr.contains("preserve-dist"),
        "error must name context.json AND --preserve-dist as the recovery hint; got:\n{}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// Test 3c: --publish-only conflicts with --split / --merge at clap level
// ---------------------------------------------------------------------------

/// Verifies the clap-level mutual exclusion. The flag was added with
/// `conflicts_with_all = ["split", "merge"]`; passing both must trip
/// clap before any code runs.
#[test]
fn publish_only_conflicts_with_split_at_clap_level() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", "--split"])
        .output()
        .expect("invoking anodize release --publish-only --split");

    assert!(
        !output.status.success(),
        "clap should reject --publish-only --split combination"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Clap's "cannot be used with" wording is the canonical signal.
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected clap conflict error; got:\n{stderr}"
    );
}

#[test]
fn publish_only_conflicts_with_merge_at_clap_level() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", "--merge"])
        .output()
        .expect("invoking anodize release --publish-only --merge");

    assert!(
        !output.status.success(),
        "clap should reject --publish-only --merge combination"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected clap conflict error; got:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 3d: pre-flight credential check fires before mutation
// ---------------------------------------------------------------------------

/// Without `--dry-run`, `--publish-only` must bail on missing
/// credentials BEFORE any state mutation (load, strip, sign). The
/// config-derived environment preflight in `commands/release/mod.rs`
/// is the sole gate: it collects the github-release token ladder from
/// the fixture's `release:` block and aborts citing the missing env
/// var NAMES plus the surfaces that demand them.
///
/// The assertions pin its exact output — the var list, the needed-by
/// attribution, and the abort bail.
#[test]
fn publish_only_preflight_credentials_required_in_non_dry_run() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_preflight_credentials_required: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);
    let commit = head_commit(repo);
    bootstrap_preserved_dist(repo, "0.1.0", &commit);
    tag_head(repo, "0.1.0");

    // Drop EVERY token + sign env var that would let the preflight
    // pass. `env_clear` is too aggressive (kills PATH on Windows); the
    // explicit removes are surgical.
    //
    // Deliberately do NOT pass `--no-preflight`: it suppresses both
    // preflight layers, defeating this test.
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only"])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only (no creds)");

    assert!(
        !output.status.success(),
        "expected non-zero exit when credentials missing in non-dry-run mode"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "none of the env var(s) [ANODIZER_GITHUB_TOKEN, GITHUB_TOKEN] is set and non-empty"
        ),
        "env preflight must name every missing token env var; got:\n{stderr}"
    );
    assert!(
        stderr.contains("[needed by: stage:release, publish:github-release]"),
        "env preflight must attribute the requirement to its consumers; got:\n{stderr}"
    );
    assert!(
        stderr.contains("environment failure(s)"),
        "env preflight must abort with its failure bail; got:\n{stderr}"
    );
    assert!(
        !stderr.contains("missing release token"),
        "the env preflight must pre-empt the publish-only credential gate; got:\n{stderr}"
    );
}

/// `--no-preflight` must suppress the credential preflight (operator
/// opt-out for the rare case where they want the mid-pipeline failure
/// to surface instead). Without this, `--no-preflight` would only skip
/// the publisher-state preflight, which is inconsistent
/// operator-facing behavior.
#[test]
fn publish_only_no_preflight_suppresses_credential_check() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_no_preflight_suppresses_credential_check: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);
    let commit = head_commit(repo);
    bootstrap_preserved_dist(repo, "0.1.0", &commit);
    tag_head(repo, "0.1.0");

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", "--no-preflight"])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only --no-preflight");

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let merged = format!("{stdout}\n{stderr}");

    // The preflight warn-line must fire (so the operator sees they
    // dropped the safety net) and the run must proceed past the env
    // preflight. Run will fail later when the actual sign / release
    // stages try to use the missing creds — we only assert the bypass
    // log line here.
    assert!(
        merged.contains("preflight skipped via --no-preflight"),
        "expected --no-preflight warn line; output was:\n{merged}"
    );
    // And the run must NOT bail with the env-preflight failure.
    assert!(
        !merged.contains("environment failure(s)"),
        "env preflight should be suppressed by --no-preflight; output was:\n{merged}"
    );
}

// ---------------------------------------------------------------------------
// Test 3e: commit mismatch is a hard error
// ---------------------------------------------------------------------------

/// Safety property: shipping signatures over bytes from a different
/// commit must be blocked. The Phase-2 loader asserts
/// `context.commit == ctx.FullCommit`.
#[test]
fn publish_only_rejects_commit_mismatch() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_rejects_commit_mismatch: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);

    // Deliberately stamp context.json with a bogus commit SHA.
    let bogus_commit = "0".repeat(40);
    bootstrap_preserved_dist(repo, "0.1.0", &bogus_commit);
    tag_head(repo, "0.1.0");

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--publish-only", "--dry-run", "--no-preflight"])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only --dry-run");

    assert!(
        !output.status.success(),
        "expected non-zero exit on commit mismatch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("commit"),
        "error must mention commit mismatch; got:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: re-sign with production keys
// ---------------------------------------------------------------------------

/// Spec Test 4: `--publish-only` re-signs the preserved archives with
/// production keys and a verifier accepts the resulting signature.
///
/// `#[ignore]`d by default: cosign + gpg aren't reliably available
/// across the matrix of CI shards. Run with
/// `cargo test -p anodizer --test publish_only -- --ignored` on a host
/// where the tools are installed.
///
/// The test exercises the full ephemeral-strip + production-resign
/// idempotence path:
///   1. Bootstrap a preserved dist whose `dist/<archive>.sig` is a
///      stale ephemeral signature (random bytes).
///   2. Drop a `.anodizer.yaml` with a single `signs:` entry using
///      `gpg --batch --yes --detach-sign` against a fresh test keypair.
///   3. Run `--publish-only --skip=release,publish,blob,snapcraft-publish`
///      to exercise ONLY the head SignStage.
///   4. Verify the resulting `.sig` against the production public key
///      via `gpg --verify`.
///
/// Skipped silently when gpg / a HOME-isolation guard aren't present.
#[test]
#[ignore = "requires gpg on PATH; opt-in via --ignored"]
fn publish_only_resigns_with_production_keys_idempotently() {
    if !tool_on_path("gpg") || !tool_on_path("git") {
        eprintln!("SKIP publish_only_resigns_with_production_keys: gpg/git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    // Override .anodizer.yaml to add a signs: section so SignStage has
    // something to do. gpg key-id is left to "default" (gpg's first
    // secret key); the gnupghome below pins which keyring that is.
    let host = common::host_triple();
    let yaml = format!(
        r#"crates:
  - name: {crate_name}
    path: .
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - id: {crate_name}
        binary: {crate_name}
        targets:
          - {host}
signs:
  - id: gpg
    artifacts: archive
    cmd: gpg
    args:
      - --batch
      - --yes
      - --pinentry-mode
      - loopback
      - --output
      - "{{{{ .Signature }}}}"
      - --detach-sign
      - "{{{{ .Artifact }}}}"
"#,
        crate_name = FIXTURE_CRATE_NAME,
        host = host,
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    // gitignore dist/ + the gpg keyring/batch scratch this test stages
    // inside the repo dir — otherwise anodize's git-dirty check bails
    // before reaching the publish-only branch.
    fs::write(repo.join(".gitignore"), "dist/\ngnupg/\nkeygen.batch\n").unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "configure signs"]);
    let commit = head_commit(repo);

    let version = "0.1.0";
    let (archive_name, _target) = bootstrap_preserved_dist(repo, version, &commit);
    tag_head(repo, version);

    // Plant an ephemeral .sig + register it in artifacts.json so the
    // strip path has something to remove. ArtifactKind=signature.
    let sig_path = repo.join("dist").join(format!("{archive_name}.sig"));
    fs::write(&sig_path, b"EPHEMERAL_SIG\n").unwrap();
    let artifacts_path = repo.join("dist/artifacts.json");
    let mut artifacts: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifacts_path).unwrap()).unwrap();
    artifacts.as_array_mut().unwrap().push(serde_json::json!({
        "kind": "signature",
        "name": format!("{archive_name}.sig"),
        "path": sig_path.to_string_lossy(),
        "target": null,
        "crate_name": FIXTURE_CRATE_NAME,
        "metadata": {},
    }));
    fs::write(
        &artifacts_path,
        serde_json::to_string_pretty(&artifacts).unwrap(),
    )
    .unwrap();

    // Provision a fresh GPG keypair in an isolated GNUPGHOME so we
    // don't touch the operator's keyring.
    let gnupghome = tmp.path().join("gnupg");
    fs::create_dir_all(&gnupghome).unwrap();
    // chmod 700 — gpg refuses to use a homedir with permissive perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gnupghome, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let key_batch = "\
%no-protection
Key-Type: RSA
Key-Length: 2048
Name-Real: Publish-Only Test
Name-Email: publish-only@test.invalid
Expire-Date: 0
%commit
";
    let batch_path = tmp.path().join("keygen.batch");
    fs::write(&batch_path, key_batch).unwrap();
    let out = Command::new("gpg")
        .env("GNUPGHOME", &gnupghome)
        .args(["--batch", "--gen-key"])
        .arg(&batch_path)
        .output()
        .expect("gpg --gen-key");
    assert!(
        out.status.success(),
        "gpg key generation failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Run the publish-only flow restricted to the head SignStage.
    let run = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publish-only",
            "--no-preflight",
            "--skip",
            "release,publish,blob,snapcraft-publish,announce",
        ])
        .env("GNUPGHOME", &gnupghome)
        // Force preflight to pass by exporting fake env-var sentinels;
        // the head SignStage doesn't read these directly (it reads
        // gpg's keyring), but the publish-only preflight gate does.
        .env("GPG_PRIVATE_KEY", "present")
        .env("GITHUB_TOKEN", "present")
        .current_dir(repo)
        .output()
        .expect("invoking publish-only re-sign");

    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let stderr = String::from_utf8_lossy(&run.stderr).to_string();
    assert!(
        run.status.success(),
        "publish-only re-sign exited non-zero; stdout=\n{stdout}\nstderr=\n{stderr}"
    );

    // The new signature must exist AND verify against the GPG keyring.
    let new_sig = repo.join("dist").join(format!("{archive_name}.sig"));
    assert!(
        new_sig.exists(),
        "expected re-signed signature at {}",
        new_sig.display()
    );
    // The new sig must not be the ephemeral sentinel (ephemeral-strip +
    // re-sign produced a real signature blob).
    let sig_bytes = fs::read(&new_sig).unwrap();
    assert_ne!(
        sig_bytes,
        b"EPHEMERAL_SIG\n".to_vec(),
        "ephemeral signature was not replaced; SignStage failed to overwrite"
    );

    let verify = Command::new("gpg")
        .env("GNUPGHOME", &gnupghome)
        .args(["--verify"])
        .arg(&new_sig)
        .arg(repo.join("dist").join(&archive_name))
        .output()
        .expect("gpg --verify");
    assert!(
        verify.status.success(),
        "gpg --verify rejected the re-signed signature: stdout={} stderr={}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );

    // Tear down the gpg-agent daemon `gpg --gen-key` spawned. When
    // tempdir drops, the daemon is left holding the (now-deleted)
    // GNUPGHOME socket — over many runs this leaks processes /
    // sockets in CI. `gpgconf --kill all` is the canonical shutdown
    // path. Tolerate missing gpgconf: hosts where this `#[ignore]`d
    // test runs all carry gpgconf alongside gpg, but if a future
    // host doesn't, leaking a stale daemon is no worse than the
    // pre-cleanup baseline.
    let _ = Command::new("gpgconf")
        .env("GNUPGHOME", &gnupghome)
        .args(["--kill", "all"])
        .status()
        .ok();
}

// ---------------------------------------------------------------------------
// Shard-merge union: publish-only must surface BOTH shards' artifacts
// ---------------------------------------------------------------------------

/// Pinned sha256 hex of `SHARD_A_PAYLOAD` (22 bytes). Computed
/// out-of-band — verifying the literal here would only re-derive the
/// value via the same hash function that the production code uses to
/// satisfy `hash_verify_preserved_dist`, making the test tautological.
/// Hand-pinned values force a real mismatch surface if the planted
/// bytes ever drift.
const SHARD_A_SHA256: &str = "77831ad6a6f50f547137457e01f5013db8ab0850f1e7ac3377cf79531973e2ec";
const SHARD_B_SHA256: &str = "a67a65f266ffc9cd4acd19945642328a831e1076fe2c30772c881a1f3b6bb974";
const SHARD_A_PAYLOAD: &[u8] = b"SHARD_A_ARCHIVE_BYTES\n";
const SHARD_B_PAYLOAD: &[u8] = b"SHARD_B_ARCHIVE_BYTES\n";

/// Plant a per-shard `artifacts-<shard>.json` + matching
/// `context-<shard>.json` pair plus the on-disk archive bytes the
/// hash-verify step demands. Returns the archive filename so callers
/// can grep for it in the post-pipeline `dist/artifacts.json`.
///
/// The two shards plant DIFFERENT archive paths so a regression that
/// collapsed the merge to a single-shard view would lose one of them
/// — i.e. the duplicate-path detection isn't what catches the bug,
/// the union assertion is.
fn plant_shard_manifest(
    dist: &Path,
    shard: &str,
    target: &str,
    version: &str,
    commit: &str,
    sha256_hex: &str,
    payload: &[u8],
) -> String {
    let archive_name = format!("{FIXTURE_CRATE_NAME}_{version}_{shard}_{target}.tar.gz");
    let archive_path = dist.join(&archive_name);
    fs::write(&archive_path, payload).unwrap();

    let artifacts_json = serde_json::json!([
        {
            "kind": "archive",
            "name": archive_name,
            "path": archive_path.to_string_lossy(),
            "target": target,
            "crate_name": FIXTURE_CRATE_NAME,
            "metadata": {
                "ID": FIXTURE_CRATE_NAME,
                "Format": "tar.gz",
                "sha256": sha256_hex,
            },
        }
    ]);
    fs::write(
        dist.join(format!("artifacts-{shard}.json")),
        serde_json::to_string_pretty(&artifacts_json).unwrap(),
    )
    .unwrap();

    let context_json = serde_json::json!({
        "artifacts": [
            {
                "name": archive_name,
                "path": archive_name,
                "sha256": format!("sha256:{sha256_hex}"),
                "size": payload.len() as u64,
            }
        ],
        "targets": [target],
        "version": version,
        "commit": commit,
    });
    fs::write(
        dist.join(format!("context-{shard}.json")),
        serde_json::to_string_pretty(&context_json).unwrap(),
    )
    .unwrap();

    archive_name
}

/// Loading per-shard `artifacts-<shard>.json` manifests unions every
/// shard's entries into the registry, so the rewritten
/// `dist/artifacts.json` carries each archive with its `sha256`,
/// `kind`, `target`, and `crate_name` preserved.
#[test]
fn publish_only_unions_sha256_across_sharded_manifests() {
    if !tool_on_path("git") {
        eprintln!("SKIP publish_only_unions_sha256_across_sharded_manifests: git missing");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);
    configure_tag_template(repo);

    let version = "0.1.0";
    let commit = head_commit(repo);
    let dist = repo.join("dist");
    fs::create_dir_all(&dist).unwrap();

    let target = "x86_64-unknown-linux-gnu";
    let archive_a = plant_shard_manifest(
        &dist,
        "shard-a",
        target,
        version,
        &commit,
        SHARD_A_SHA256,
        SHARD_A_PAYLOAD,
    );
    let archive_b = plant_shard_manifest(
        &dist,
        "shard-b",
        target,
        version,
        &commit,
        SHARD_B_SHA256,
        SHARD_B_PAYLOAD,
    );
    assert_ne!(
        archive_a, archive_b,
        "fixture must plant DISTINCT archive paths per shard so a single-shard \
         collapse loses one entry; otherwise duplicate-path detection masks the bug"
    );

    tag_head(repo, version);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publish-only",
            "--dry-run",
            "--no-preflight",
            "--skip",
            "announce",
        ])
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .current_dir(repo)
        .output()
        .expect("invoking anodize release --publish-only --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let merged_log = format!("{stdout}\n{stderr}");

    assert!(
        output.status.success(),
        "expected zero exit from publish-only with two shard manifests; \
         stdout=\n{stdout}\nstderr=\n{stderr}",
    );

    // The publish-only banner must announce TWO artifacts manifests
    // were loaded — a regression that silently picked one would log
    // "1 artifacts manifest(s)" and fail this assertion before the
    // post-pipeline file even matters.
    assert!(
        merged_log.contains("from 2 artifacts manifest(s)"),
        "publish-only must report loading exactly 2 shard manifests; output was:\n{merged_log}"
    );

    // Post-pipeline-rewritten artifacts.json is the load-bearing
    // assertion: it's what the next consumer of `dist/` (a re-run, a
    // downstream `anodize publish` invocation, or operator inspection)
    // sees, and a regression that dropped a shard would surface here
    // as a missing entry.
    let post_artifacts = dist.join("artifacts.json");
    assert!(
        post_artifacts.is_file(),
        "expected run_post_pipeline to rewrite {} after publish-only succeeded",
        post_artifacts.display(),
    );

    #[derive(serde::Deserialize, Debug)]
    struct PostArtifact {
        kind: String,
        name: String,
        path: String,
        #[serde(default)]
        target: Option<String>,
        crate_name: String,
    }
    let bytes = fs::read_to_string(&post_artifacts)
        .unwrap_or_else(|e| panic!("read {}: {e}", post_artifacts.display()));
    let parsed: Vec<PostArtifact> = serde_json::from_str(&bytes).unwrap_or_else(|e| {
        panic!(
            "parse {} as Vec<PostArtifact>: {e}",
            post_artifacts.display()
        )
    });

    let archive_entries: Vec<&PostArtifact> =
        parsed.iter().filter(|a| a.kind == "archive").collect();
    let names: Vec<&str> = archive_entries.iter().map(|a| a.name.as_str()).collect();

    assert!(
        names.contains(&archive_a.as_str()),
        "shard-a archive {archive_a} missing from post-pipeline artifacts.json; \
         survivors were: {names:?}"
    );
    assert!(
        names.contains(&archive_b.as_str()),
        "shard-b archive {archive_b} missing from post-pipeline artifacts.json; \
         survivors were: {names:?}"
    );
    assert_eq!(
        archive_entries.len(),
        2,
        "expected EXACTLY two archive entries (one per shard); a regression that \
         deduped them or dropped a shard would fail here. Got entries: {archive_entries:?}"
    );

    for entry in &archive_entries {
        assert_eq!(entry.target.as_deref(), Some(target));
        assert_eq!(entry.crate_name, FIXTURE_CRATE_NAME);
        assert!(
            entry.path.ends_with(&entry.name),
            "archive path {} must end with its filename {}",
            entry.path,
            entry.name,
        );
    }

    // Per-shard manifests must be removed once the canonical
    // un-suffixed artifacts.json is rewritten — otherwise a retry
    // (operator-driven workflow rerun) trips the unsuffixed-vs-
    // suffixed collision check. The cleanup is part of the merge
    // contract too: without it the file the next run reads would
    // collide with the surviving sharded files.
    for shard in ["shard-a", "shard-b"] {
        let shard_manifest = dist.join(format!("artifacts-{shard}.json"));
        assert!(
            !shard_manifest.exists(),
            "shard manifest {} must be cleaned up after successful publish-only",
            shard_manifest.display(),
        );
    }
}
