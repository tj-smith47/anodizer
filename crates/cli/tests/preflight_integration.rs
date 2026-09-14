//! Integration tests for `anodizer preflight` (the config-derived
//! environment preflight command).
//!
//! Drives the real binary against a synthesized fixture repo whose config
//! demands things the host cannot satisfy, asserting:
//!   - collect-all: failures from independent surfaces (publisher SSH key,
//!     sbom tool, cosign key material) all appear in ONE run;
//!   - the exit code is non-zero when anything is missing;
//!   - secret VALUES never appear in the output — only env-var names;
//!   - `--json` emits a machine-readable report with the same failures.
//!
//! Skips cleanly on hosts without git (fixture bootstrap needs it), same
//! convention as `publish_only.rs`.
//!
//! The reconcile-sweep block at the bottom drives the same binary against a
//! LOCAL package feed, so the whole "does this version's publisher state gate
//! the run?" decision — position resolution, sweep skip, probe, exit code —
//! is asserted end to end without touching a real registry.

use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, run_git, tool_on_path};

const FIXTURE_CRATE_NAME: &str = "anodizer-preflight-fixture";

/// Sentinel that must NEVER appear in preflight output: it is the VALUE of
/// an env var the config requires (a malformed cosign key, so the check
/// fails and the failure message is exercised, not just the happy path).
const SECRET_SENTINEL: &str = "SUPERSECRET-PREFLIGHT-SENTINEL-VALUE";

fn write_fixture_config(dir: &std::path::Path) {
    // Four independent failure surfaces:
    //   publish.aur  -> PF_MISSING_AUR_KEY unset        (env-missing)
    //   publish.npm  -> NPM_TOKEN unset                 (env-missing)
    //   sboms.cmd    -> tool that cannot exist on PATH  (tool-missing)
    //   signs env:// -> PF_COSIGN_KEY set but malformed (bad key material)
    //
    // aur and npm are BOTH publishers, so a `--publishers` allowlist can
    // select one and deselect the other in a single run — the allowlist test
    // asserts the selected publisher's requirement survives while the
    // deselected one drops.
    let yaml = format!(
        r#"project_name: {FIXTURE_CRATE_NAME}
crates:
  - name: {FIXTURE_CRATE_NAME}
    path: .
    publish:
      aur:
        private_key: "{{{{ .Env.PF_MISSING_AUR_KEY }}}}"
npms:
  - scope: "@pf"
uploads:
  - name: mirror
    target: "https://uploads.example/{{{{ .ProjectName }}}}/{{{{ .ArtifactName }}}}"
    signature: true
signs:
  - artifacts: checksum
    cmd: cosign
    args: ["sign-blob", "--key", "env://PF_COSIGN_KEY", "{{{{ .Artifact }}}}"]
binary_signs:
  - cmd: cosign
    args: ["sign-blob", "--key", "env://PF_BINARY_COSIGN_KEY", "{{{{ .Artifact }}}}"]
sboms:
  - cmd: pf-definitely-not-a-real-tool-9z
"#
    );
    std::fs::write(dir.join(".anodizer.yaml"), yaml).unwrap();
}

fn run_preflight(dir: &std::path::Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(dir)
        .arg("preflight")
        .args(extra_args)
        // The malformed-but-SET secrets: their NAMEs may appear, VALUEs must not.
        // PF_COSIGN_KEY backs the `signs:` slice; PF_BINARY_COSIGN_KEY backs the
        // `binary_signs:` slice — distinct env vars so each slice's gate is
        // asserted independently.
        .env("PF_COSIGN_KEY", SECRET_SENTINEL)
        .env("PF_BINARY_COSIGN_KEY", SECRET_SENTINEL)
        .env_remove("PF_MISSING_AUR_KEY")
        // Unset on the child only (never the test process) so the npm
        // publisher's token requirement deterministically reads as missing;
        // the allowlist test asserts this surfaces for the SELECTED npm
        // publisher. Per-child env keeps the test-isolation guard satisfied.
        .env_remove("NPM_TOKEN")
        .output()
        .expect("spawn anodizer preflight")
}

#[test]
fn preflight_collects_all_failures_and_exits_nonzero() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !out.status.success(),
        "preflight must exit non-zero on failures; output:\n{combined}"
    );
    // Collect-all: every independent failure surface present in ONE run.
    assert!(
        combined.contains("PF_MISSING_AUR_KEY"),
        "missing publisher SSH key env var not reported:\n{combined}"
    );
    assert!(
        combined.contains("pf-definitely-not-a-real-tool-9z"),
        "missing sbom tool not reported:\n{combined}"
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "malformed cosign key env var not reported:\n{combined}"
    );
    // Secret hygiene: the VALUE of the set-but-invalid key never leaks.
    assert!(
        !combined.contains(SECRET_SENTINEL),
        "preflight output echoed a secret value:\n{combined}"
    );
}

#[test]
fn preflight_json_reports_same_failures() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--json"]);
    assert!(!out.status.success(), "non-zero exit expected");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_start = stdout.find('{').expect("JSON object in stdout");
    let report: serde_json::Value =
        serde_json::from_str(stdout[json_start..].trim()).expect("valid JSON report");
    let failures = report["failures"].as_array().expect("failures array");
    assert!(
        failures.len() >= 3,
        "expected at least 3 failures, got: {failures:?}"
    );
    let kinds: Vec<&str> = failures.iter().filter_map(|f| f["kind"].as_str()).collect();
    assert!(kinds.contains(&"missing_env"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"missing_tool"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"bad_key_material"), "kinds: {kinds:?}");
    assert!(
        !stdout.contains(SECRET_SENTINEL),
        "JSON output echoed a secret value"
    );
}

#[test]
fn preflight_skip_drops_stage_requirements() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--skip=sign,sbom,publish"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("pf-definitely-not-a-real-tool-9z"),
        "skipped sbom stage still contributed requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_COSIGN_KEY"),
        "skipped sign stage still contributed signs requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_BINARY_COSIGN_KEY"),
        "skipped sign stage still contributed binary_signs requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_MISSING_AUR_KEY"),
        "skipped publish still contributed requirements:\n{combined}"
    );
}

/// `preflight --publishers <name>` mirrors `release --publishers`: a
/// non-empty allowlist SELECTS the named publisher and DESELECTS every other
/// one, in a single pass. The fixture configures two publishers — `npm` and
/// `aur` — so one run proves BOTH directions of the allowlist:
///   - SELECTED (`npm`): its `NPM_TOKEN` requirement SURVIVES the allowlist
///     and surfaces in the report as `[needed by: publish:npm]`;
///   - DESELECTED (`aur`): its `PF_MISSING_AUR_KEY` requirement is DROPPED.
///
/// Crucially, NO `--skip` is passed: the `--publishers npm` allowlist ALONE
/// must auto-deselect the `signs:` surface (its only consumers —
/// github-release / blob / artifactory — are all deselected), so the malformed
/// `PF_COSIGN_KEY` cosign demand vanishes without a hand-skip. This is exactly
/// the surface the npm-provenance job validates with
/// `preflight --publish-only --publishers npm` and zero `--skip`.
#[test]
fn preflight_publishers_allowlist_keeps_selected_drops_deselected_publisher() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // Allowlist `npm` with NO `--skip`: npm is SELECTED (its token requirement
    // must survive), the configured-but-unselected `aur` publisher is
    // DESELECTED (its key requirement must vanish), and the `signs:` slice
    // self-deselects because every signature consumer is deselected.
    let out = run_preflight(tmp.path(), &["--publish-only", "--publishers=npm"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // SELECTED publisher: requirement survives the allowlist AND is attributed
    // to the npm publisher source, not merely present by coincidence.
    assert!(
        combined.contains("NPM_TOKEN"),
        "allowlist-selected npm publisher's token requirement was dropped:\n{combined}"
    );
    assert!(
        combined.contains("publish:npm"),
        "npm token requirement not attributed to the selected npm publisher:\n{combined}"
    );
    // DESELECTED publisher: its requirement is gone.
    assert!(
        !combined.contains("PF_MISSING_AUR_KEY"),
        "allowlist-deselected aur publisher still demanded its key:\n{combined}"
    );
    assert!(
        !combined.contains("publish:aur"),
        "deselected aur publisher still attributed a requirement source:\n{combined}"
    );
    // The signs slice self-deselects (no consumer selected) WITHOUT a
    // hand-skip, so its cosign key demand is gone.
    assert!(
        !combined.contains("PF_COSIGN_KEY"),
        "--publishers npm must auto-deselect the signs surface (no --skip):\n{combined}"
    );
    // The binary_signs slice hangs off the SAME consumer set, so its DISTINCT
    // cosign key demand is ALSO gone WITHOUT a hand-skip — this is the second
    // half of the npm-clean invariant.
    assert!(
        !combined.contains("PF_BINARY_COSIGN_KEY"),
        "--publishers npm must auto-deselect the binary_signs surface (no --skip):\n{combined}"
    );
    // Neither sign slice contributes anything: every signature consumer is
    // deselected, so no `stage:sign` requirement may appear at all.
    assert!(
        !combined.contains("stage:sign"),
        "the sign slices must contribute nothing under --publish-only --publishers npm:\n{combined}"
    );
    assert!(
        !combined.contains("stage:release") && !combined.contains("publish:github-release"),
        "github-release must auto-deselect under --publishers npm (no --skip):\n{combined}"
    );
}

/// Under `--publish-only` with an EMPTY `--publishers` allowlist BOTH signature
/// surfaces must survive: `publisher_deselected` short-circuits to the denylist,
/// which never names a signature consumer, and a binary signature is a release
/// asset of the same job. Guards the gate against over-firing and silently
/// shipping a release with no signatures.
#[test]
fn preflight_publish_only_empty_allowlist_keeps_both_sign_surfaces() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // No allowlist, no skip, publish-only: neither slice's consumers are
    // deselected, so both cosign key demands (malformed PF_COSIGN_KEY and
    // PF_BINARY_COSIGN_KEY) surface.
    let out = run_preflight(tmp.path(), &["--publish-only"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "publish-only empty allowlist must keep the signs surface:\n{combined}"
    );
    assert!(
        combined.contains("PF_BINARY_COSIGN_KEY"),
        "publish-only empty allowlist must keep the binary_signs surface:\n{combined}"
    );
}

/// The MAIN-job invariant under the REAL binary: the full release pipeline
/// (no `--publish-only`; the main job runs `release --skip=npm`, i.e. the FULL
/// scope with an empty allowlist) must KEEP BOTH sign surfaces — `signs:` AND
/// `binary_signs:` — so the binaries that ship are still signed.
#[test]
fn preflight_full_scope_keeps_both_sign_surfaces() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // FULL scope (no --publish-only), empty allowlist: BOTH sign slices run.
    let out = run_preflight(tmp.path(), &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "full-scope run must keep the signs surface:\n{combined}"
    );
    assert!(
        combined.contains("PF_BINARY_COSIGN_KEY"),
        "full-scope run must keep the binary_signs surface (main-job binary signing preserved):\n{combined}"
    );
}

/// The `uploads` publisher consumes the `signs:` sidecars when an entry sets
/// `signature: true`, so it is a member of `signs_consumers()`. Selecting it
/// ALONE (every OTHER consumer deselected) must KEEP the `signs:` surface — its
/// cosign key demand must survive — proving preflight stays in lockstep with
/// the fixed runtime. Before the fix, `uploads` was absent from the hard-coded
/// three-consumer conjunction, so `--publishers uploads` falsely dropped
/// `stage:sign` and the selected uploads publisher would mirror an unsigned set.
#[test]
fn preflight_publishers_uploads_keeps_signs_surface() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--publish-only", "--publishers=uploads"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The selected `uploads` publisher reads the signs sidecars, so the signs
    // surface (and its cosign key demand) must NOT be deselected.
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "--publishers uploads must keep the signs surface (uploads consumes the sidecars):\n{combined}"
    );
    assert!(
        combined.contains("stage:sign"),
        "the signs slice must contribute its requirements under --publishers uploads:\n{combined}"
    );
}

// ---------------------------------------------------------------------------
// Reconcile sweep: position resolution → probe-or-skip → exit code
// ---------------------------------------------------------------------------

const RECONCILE_CRATE_NAME: &str = "anodizer-reconcile-fixture";
const RECONCILE_TAG: &str = "v0.1.0";
const RECONCILE_VERSION: &str = "0.1.0";

/// A fixture whose ONLY publisher is chocolatey, pointed at a local OData feed
/// and marked `required: true` so its verdict reaches the exit gate. `api_key`
/// is inline so the run demands no publisher secret from the environment and
/// the only thing that can drive a non-zero exit is the reconcile verdict.
fn write_reconcile_fixture_config(dir: &std::path::Path, feed: &str) {
    let yaml = format!(
        r#"project_name: {RECONCILE_CRATE_NAME}
crates:
  - name: {RECONCILE_CRATE_NAME}
    path: .
    tag_template: "v{{{{ .Version }}}}"
    publish:
      chocolatey:
        required: true
        api_key: fixture-key
        source_repo: "{feed}"
"#
    );
    std::fs::write(dir.join(".anodizer.yaml"), yaml).unwrap();
}

/// An OData row for [`RECONCILE_VERSION`] in the REJECTED moderation state —
/// the one feed shape chocolatey's `reconcile()` maps to `diverged`, and
/// therefore the one that must reach the exit gate when the sweep applies.
fn rejected_feed_response() -> String {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<entry>
  <id>http://example.com/api/v2/Packages(Id='{RECONCILE_CRATE_NAME}',Version='{RECONCILE_VERSION}')</id>
  <m:properties>
    <d:PackageHash>deadbeef==</d:PackageHash>
    <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>
    <d:PackageStatus>Rejected</d:PackageStatus>
    <d:IsApproved>false</d:IsApproved>
  </m:properties>
</entry>"#
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Bootstrap a repo whose tag `v0.1.0` sits `commits_after` commits behind
/// HEAD, with the reconcile fixture config pointed at a local feed that answers
/// every request with a REJECTED row. Returns the temp dir and the feed's
/// request counter.
fn reconcile_fixture(
    commits_after: usize,
) -> (TempDir, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), RECONCILE_CRATE_NAME);
    // One run asks the feed up to three times — the publisher-state probe,
    // the credential probe against the service document, and the reconcile
    // sweep — plus one spare, which keeps a retry from falling through to the
    // drain phase's 503 that would read as "absent" and quietly turn a
    // divergence assertion into a false pass.
    let (addr, calls) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with(|_| {
            vec![rejected_feed_response(); 4]
        });
    write_reconcile_fixture_config(tmp.path(), &format!("http://{addr}"));
    run_git(tmp.path(), &["add", "-A"]);
    run_git(
        tmp.path(),
        &["commit", "-q", "-m", "reconcile fixture config"],
    );
    run_git(tmp.path(), &["tag", RECONCILE_TAG]);
    for i in 0..commits_after {
        run_git(
            tmp.path(),
            &["commit", "-q", "--allow-empty", "-m", &format!("after-{i}")],
        );
    }
    (tmp, calls)
}

/// Run the reconcile fixture's preflight, returning `(output, reconcile rows)`.
fn run_reconcile_preflight(
    dir: &std::path::Path,
    env: &[(&str, &str)],
) -> (std::process::Output, Vec<serde_json::Value>) {
    let (out, report) = run_reconcile_preflight_json(dir, env);
    let rows = report["reconcile"]
        .as_array()
        .expect("reconcile array")
        .clone();
    (out, rows)
}

/// Run the reconcile fixture's preflight, returning the whole `--json` report.
fn run_reconcile_preflight_json(
    dir: &std::path::Path,
    env: &[(&str, &str)],
) -> (std::process::Output, serde_json::Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_anodizer"));
    cmd.current_dir(dir)
        .args(["preflight", "--json", "--publish-only"])
        .arg("--publishers=chocolatey");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn anodizer preflight");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let json_start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("no JSON object in stdout: {stdout}"));
    let report: serde_json::Value =
        serde_json::from_str(stdout[json_start..].trim()).expect("valid JSON report");
    (out, report)
}

/// End-to-end, HEAD one `feat:` commit PAST `v0.1.0`: the version this tree
/// would release is the planned `0.2.0`, so that is what both the
/// publisher-state probe and the sweep ask the feed about — never the `0.1.0`
/// the context resolved from the last tag. The planned tag does not exist, so
/// the sweep applies and probes it.
#[test]
fn preflight_probes_the_planned_version_on_an_untagged_head() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, calls) = reconcile_fixture(0);
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: something new"],
    );
    let (out, report) = run_reconcile_preflight_json(tmp.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    let entries = report["publishers"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("publisher entries in the JSON report: {report}"));
    assert_eq!(entries.len(), 1, "one chocolatey entry: {entries:?}");
    assert_eq!(entries[0]["publisher"], "chocolatey");
    assert_eq!(
        entries[0]["version"], "0.2.0",
        "the publisher probe must ask about the planned version; stderr:\n{stderr}"
    );
    let rows = report["reconcile"].as_array().expect("reconcile array");
    assert_eq!(rows.len(), 1, "expected one publisher row, got: {rows:?}");
    assert_eq!(
        rows[0]["publisher"], "chocolatey",
        "the sweep must probe the planned version: {rows:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the publisher state probe, the credential probe and the sweep each ask the feed once"
    );
}

/// The previous tag is what the REMOTE still carries, as it is for
/// `anodizer tag`: a tag deleted on the remote for a re-cut survives in this
/// clone, and a plan read off local tags alone would bump past the version
/// the remote will cut. `v0.2.0` is gone from the remote, so the plan bumps
/// from `v0.1.0` and arrives at `0.2.0` again.
#[test]
fn preflight_plans_from_the_tags_the_remote_still_has() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (origin, _calls) = reconcile_fixture(0);
    run_git(
        origin.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: one"],
    );
    run_git(origin.path(), &["tag", "v0.2.0"]);

    let clone = TempDir::new().unwrap();
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["clone", "-q"])
                .arg(origin.path())
                .arg(clone.path());
            cmd
        },
        "git",
    );
    assert!(out.status.success(), "clone: {out:?}");
    run_git(clone.path(), &["config", "user.email", "test@test.com"]);
    run_git(clone.path(), &["config", "user.name", "Test"]);
    run_git(clone.path(), &["config", "commit.gpgsign", "false"]);
    run_git(origin.path(), &["tag", "-d", "v0.2.0"]);
    run_git(
        clone.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: two"],
    );

    let (out, report) = run_reconcile_preflight_json(clone.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let entries = report["publishers"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("publisher entries in the JSON report: {report}"));
    assert_eq!(
        entries[0]["version"], "0.2.0",
        "the plan must bump from the remote's newest tag; stderr:\n{stderr}"
    );
}

/// A chocolatey fixture whose feed answers 404 to everything, for the tests
/// about the version derivation, where the feed is beside the point: the sweep reads
/// `absent`, and no env half requirement is missing.
fn quiet_choco_fixture() -> (TempDir, std::net::SocketAddr) {
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), RECONCILE_CRATE_NAME);
    let (addr, _requests) =
        anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder(vec![]);
    write_reconcile_fixture_config(tmp.path(), &format!("http://{addr}"));
    run_git(tmp.path(), &["add", "-A"]);
    (tmp, addr)
}

fn run_verbose_preflight(dir: &std::path::Path) -> (std::process::Output, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(dir)
        .args([
            "preflight",
            "--verbose",
            "--publish-only",
            "--publishers=chocolatey",
        ])
        .output()
        .expect("spawn anodizer preflight");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (out, stderr)
}

/// A repository with no commits still gets a report: the git-info failure
/// takes the same lenient arm nightly and snapshot take, the plan fails on
/// the empty log and says so, and the sweep still runs on the first version.
#[test]
fn preflight_reports_on_a_repository_with_no_commits() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, _addr) = quiet_choco_fixture();
    // Unborn branch: the files stay in the index, the log is empty.
    run_git(tmp.path(), &["update-ref", "-d", "refs/heads/master"]);
    let (out, stderr) = run_verbose_preflight(tmp.path());
    assert!(
        stderr.contains("could not detect git info in preflight mode, using defaults:"),
        "the lenient arm must take over; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("could not plan the next version:")
            && stderr.contains("publisher probes use the current version 0.0.0"),
        "the plan arm must degrade; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Reconcile state") && stderr.contains("chocolatey"),
        "the report must still print; stderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "no git error may abort the command; status {:?}; stderr:\n{stderr}",
        out.status
    );
}

/// A tag git cannot place (here a tag on a blob) keeps the current
/// version: the note is printed and the report still follows.
#[test]
fn preflight_keeps_the_current_version_when_the_tag_cannot_be_placed() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, _addr) = quiet_choco_fixture();
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    let blob = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["rev-parse", "HEAD:Cargo.toml"])
                .current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let blob = String::from_utf8_lossy(&blob.stdout).trim().to_string();
    run_git(tmp.path(), &["tag", RECONCILE_TAG, &blob]);
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: one"],
    );
    let (out, stderr) = run_verbose_preflight(tmp.path());
    assert!(
        stderr.contains(&format!(
            "could not locate tag {RECONCILE_TAG} relative to HEAD:"
        )) && stderr.contains(&format!(
            "publisher probes use the current version {RECONCILE_VERSION}"
        )),
        "the tag-position arm must degrade; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Reconcile state") && stderr.contains("chocolatey"),
        "the report must still print; stderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "status {:?}; stderr:\n{stderr}",
        out.status
    );
}

/// A plan that fails keeps the current version: the empty repository above
/// covers the git failure inside `plan_next_version`; this one is the same
/// arm with a tag in place and HEAD past it, so the plan is really asked.
#[test]
fn preflight_keeps_the_current_version_when_the_plan_fails() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, _addr) = quiet_choco_fixture();
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    run_git(tmp.path(), &["tag", RECONCILE_TAG]);
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: one"],
    );
    // The plan reads the commit log through git's own `log`, which refuses
    // a `log.showSignature` it cannot parse; the context build never reads
    // that key, so only the plan fails.
    run_git(tmp.path(), &["config", "log.showSignature", "not-a-bool"]);
    let (out, stderr) = run_verbose_preflight(tmp.path());
    assert!(
        stderr.contains("could not plan the next version:")
            && stderr.contains(&format!(
                "publisher probes use the current version {RECONCILE_VERSION}"
            )),
        "the plan arm must degrade; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Reconcile state") && stderr.contains("chocolatey"),
        "the report must still print; stderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "status {:?}; stderr:\n{stderr}",
        out.status
    );
}

/// An `origin` that cannot be listed falls back to local tags with a note,
/// and the plan still bumps.
#[test]
fn preflight_plans_from_local_tags_when_the_remote_cannot_be_listed() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, _addr) = quiet_choco_fixture();
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    run_git(tmp.path(), &["tag", RECONCILE_TAG]);
    run_git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: one"],
    );
    let missing = tmp.path().join("no-such-remote.git");
    run_git(
        tmp.path(),
        &["remote", "add", "origin", &missing.display().to_string()],
    );
    let (out, stderr) = run_verbose_preflight(tmp.path());
    assert!(
        stderr.contains("could not list tags on remote 'origin'")
            && stderr.contains("the plan reads local tags"),
        "the fallback note must print; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("publisher probes use the planned version 0.2.0 (v0.1.0 → v0.2.0)"),
        "local tags must still plan the bump; stderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "status {:?}; stderr:\n{stderr}",
        out.status
    );
}

/// One `cargo publish --dry-run` per selected crate: a two-crate lockstep
/// workspace spawns exactly two, one for each crate, in either order.
#[test]
fn preflight_runs_one_cargo_publish_simulation_per_crate() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"fx-alpha\", \"fx-beta\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for name in ["fx-alpha", "fx-beta"] {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion.workspace = true\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    }
    std::fs::write(
        tmp.path().join(".anodizer.yaml"),
        r#"project_name: fx
crates:
  - name: fx-alpha
    path: fx-alpha
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
  - name: fx-beta
    path: fx-beta
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#,
    )
    .unwrap();
    run_git(tmp.path(), &["init", "-q"]);
    run_git(tmp.path(), &["config", "user.email", "test@test.com"]);
    run_git(tmp.path(), &["config", "user.name", "Test"]);
    run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    run_git(tmp.path(), &["add", "-A"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "workspace fixture"]);
    run_git(tmp.path(), &["tag", "v0.1.0"]);

    let (index, _requests) =
        anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder(vec![]);
    let tools = anodizer_core::test_helpers::fake_tool::FakeToolDir::new();
    tools.tool("cargo").stdout("cargo 1.0.0\n").install();
    let path = std::env::join_paths(std::iter::once(tools.bin_dir().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("join PATH");
    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(tmp.path())
        .args([
            "preflight",
            "--verbose",
            "--publish-only",
            "--publishers=cargo",
        ])
        .env("PATH", path)
        .env("ANODIZE_TEST_HARNESS", "1")
        .env(
            "ANODIZER_TEST_CRATES_IO_INDEX_BASE",
            format!("http://{index}"),
        )
        .env_remove("CARGO_REGISTRY_TOKEN")
        .output()
        .expect("spawn anodizer preflight");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    let mut dry_runs: Vec<Vec<String>> = tools
        .calls("cargo")
        .into_iter()
        .filter(|argv| argv.first().map(String::as_str) == Some("publish"))
        .collect();
    dry_runs.sort();
    let expected = |name: &str| {
        vec![
            "publish".to_string(),
            "--dry-run".to_string(),
            "-p".to_string(),
            name.to_string(),
        ]
    };
    assert_eq!(
        dry_runs,
        vec![expected("fx-alpha"), expected("fx-beta")],
        "one cargo publish --dry-run per crate; stderr:\n{stderr}"
    );
}

/// The publisher half runs live inside the standalone: the crates.io state
/// probe and the `cargo publish --dry-run` simulation both fire, the
/// simulation exactly once per crate. A context that presented itself as a
/// dry run skipped both, and a release invoked with `--skip=preflight`
/// relies on this command for them.
#[test]
fn preflight_runs_the_cargo_publish_simulation_once() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    std::fs::write(
        tmp.path().join(".anodizer.yaml"),
        format!(
            r#"project_name: {FIXTURE_CRATE_NAME}
crates:
  - name: {FIXTURE_CRATE_NAME}
    path: .
    tag_template: "v{{{{ .Version }}}}"
    publish:
      cargo: {{}}
"#
        ),
    )
    .unwrap();
    run_git(tmp.path(), &["add", "-A"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "cargo fixture config"]);
    run_git(tmp.path(), &["tag", "v0.1.0"]);

    // Every index route answers 404: nothing is published, so the simulation
    // reaches its dry-run step.
    let (index, _requests) =
        anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder(vec![]);
    let tools = anodizer_core::test_helpers::fake_tool::FakeToolDir::new();
    tools.tool("cargo").stdout("cargo 1.0.0\n").install();
    let path = std::env::join_paths(std::iter::once(tools.bin_dir().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("join PATH");

    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(tmp.path())
        .args([
            "preflight",
            "--verbose",
            "--publish-only",
            "--publishers=cargo",
        ])
        .env("PATH", path)
        .env("ANODIZE_TEST_HARNESS", "1")
        .env(
            "ANODIZER_TEST_CRATES_IO_INDEX_BASE",
            format!("http://{index}"),
        )
        .env_remove("CARGO_REGISTRY_TOKEN")
        .output()
        .expect("spawn anodizer preflight");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    let dry_runs: Vec<Vec<String>> = tools
        .calls("cargo")
        .into_iter()
        .filter(|argv| argv.first().map(String::as_str) == Some("publish"))
        .collect();
    assert_eq!(
        dry_runs,
        vec![vec![
            "publish".to_string(),
            "--dry-run".to_string(),
            "-p".to_string(),
            FIXTURE_CRATE_NAME.to_string()
        ]],
        "one cargo publish --dry-run per crate; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("checking cargo for '{FIXTURE_CRATE_NAME}@0.1.0'")),
        "the crates.io state probe must run; stderr:\n{stderr}"
    );
}

/// A nightly release asks the registries about the NIGHTLY version: the
/// version rewrite runs before the engine, so the state probe never asks
/// about the base tag the nightly derives from.
#[test]
fn release_nightly_probes_the_nightly_version() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, _calls) = reconcile_fixture(0);
    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(tmp.path())
        .args([
            "release",
            "--nightly",
            "--verbose",
            "--skip=build",
            "--publishers=chocolatey",
        ])
        .env("GITHUB_TOKEN", "dummy-token-for-preflight-test")
        .env("ANODIZER_GITHUB_API_BASE", "http://127.0.0.1:1")
        .env_remove("GH_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .output()
        .expect("spawn anodizer release --nightly");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let probe = stderr
        .lines()
        .find(|l| l.contains("checking chocolatey for '"))
        .unwrap_or_else(|| panic!("no chocolatey state probe line; stderr:\n{stderr}"));
    assert!(
        probe.contains(&format!("{RECONCILE_CRATE_NAME}@0.1.1-")) && probe.contains("-nightly'"),
        "the probe must name the nightly version: {probe}"
    );
}

/// End-to-end, HEAD ADVANCED PAST the tag with no release signal: the
/// resolved version is the last released one, so the sweep must not run at
/// all. The observable proof is threefold — the feed is contacted only by the
/// publisher half (its state probe and its credential probe), the table
/// reports the whole-sweep skip marker
/// instead of a publisher row, and the command exits ZERO even though that
/// publisher is required and its feed row is a rejection.
#[test]
fn reconcile_sweep_skipped_end_to_end_when_head_advanced_past_the_tag() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, calls) = reconcile_fixture(1);
    let (out, rows) = run_reconcile_preflight(tmp.path(), &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        out.status.success(),
        "a skipped sweep must not gate the exit code; output:\n{combined}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a skipped sweep must not probe the registry; only the publisher half may"
    );
    assert_eq!(rows.len(), 1, "expected one marker row, got: {rows:?}");
    assert_eq!(rows[0]["publisher"], "*");
    assert_eq!(rows[0]["state"], "skipped");
    assert_eq!(rows[0]["blocking"], false);
    assert!(
        rows[0]["detail"]
            .as_str()
            .is_some_and(|d| d.contains(RECONCILE_TAG) && d.contains("advanced past it")),
        "the marker must name the version and why it was skipped: {rows:?}"
    );
}

/// End-to-end, HEAD EXACTLY AT the tag: the resolved version IS the version
/// this run would publish, so the sweep runs, the required publisher's
/// `diverged` verdict reaches the gate, and the command exits NON-ZERO with
/// the divergence bail — not the environment bail.
#[test]
fn reconcile_sweep_probes_end_to_end_and_diverged_exits_nonzero_at_the_tag() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, calls) = reconcile_fixture(0);
    let (out, rows) = run_reconcile_preflight(tmp.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "a required publisher's divergence must exit non-zero; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("required publisher(s) diverged"),
        "the divergence bail must be what failed the run, not the environment gate: {stderr}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the publisher state probe, the credential probe and the sweep each ask the feed once"
    );
    assert_eq!(rows.len(), 1, "expected one publisher row, got: {rows:?}");
    assert_eq!(rows[0]["publisher"], "chocolatey");
    assert_eq!(rows[0]["state"], "diverged");
    assert_eq!(rows[0]["blocking"], true);
}

/// End-to-end backfill canary: HEAD is a commit past `v0.1.0`, but
/// `ANODIZER_CURRENT_TAG` DECLARES `v0.1.0` as the version this run targets.
/// The staleness inference applies only to a tag anodizer picked itself, so the
/// sweep must run and the required divergence must still gate — the same tree
/// that skips in the inferred case.
#[test]
fn reconcile_sweep_probes_end_to_end_for_an_explicitly_declared_tag() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !tool_on_path("xmllint") {
        eprintln!("skipping: xmllint not on PATH (chocolatey's tool requirement)");
        return;
    }
    let (tmp, calls) = reconcile_fixture(1);
    let (out, rows) =
        run_reconcile_preflight(tmp.path(), &[("ANODIZER_CURRENT_TAG", RECONCILE_TAG)]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "a declared tag must be probed and its divergence must gate; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("required publisher(s) diverged"),
        "the divergence bail must be what failed the run: {stderr}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "a declared tag must be probed even from a tree that has moved past it"
    );
    assert_eq!(rows.len(), 1, "expected one publisher row, got: {rows:?}");
    assert_eq!(rows[0]["publisher"], "chocolatey");
    assert_eq!(rows[0]["state"], "diverged");
}
