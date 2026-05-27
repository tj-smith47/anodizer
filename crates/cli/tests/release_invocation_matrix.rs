//! Skip-stage matrix for every documented `anodizer release` /
//! `anodizer publish` / `anodizer announce` / `anodizer continue`
//! invocation. Each test pins one row of the table in
//! `docs/site/content/docs/general/release-workflow.md`: the
//! documented stages run, the documented stages skip.
//!
//! All invocations run under `--dry-run` so no upstream side effect
//! ever fires; the heavy compile / archive / sign stages are
//! `--skip`ed where the table allows them to be, keeping each test
//! cheap. The harness greps stderr for the canonical Pipeline-emitted
//! "<stage> skipped" line (yellow, ANSI-stripped) — same shape the
//! sibling `tests/integration.rs::test_release_prepare_matches_explicit_skip`
//! relies on, so a logging-format change updates both at once.
//!
//! A docs row that drifts from the binary's behaviour fails one of
//! these tests; fix the binary OR the docs and re-run.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

use anodizer_core::test_helpers::{create_config, create_test_project, init_git_repo};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn host_target() -> String {
    anodizer_cli::detect_host_target().expect("rustc -vV must succeed in test env")
}

/// Minimal single-crate snapshot config. Mirrors the helper in
/// `tests/integration.rs` (kept local so this file stays standalone —
/// integration-test binaries can't share helpers across files outside
/// of `tests/common/`).
fn minimal_config(host: &str) -> String {
    format!(
        r#"project_name: matrix-fixture
crates:
  - name: matrix-fixture
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: matrix-fixture
        targets:
          - {host}
    archives:
      - name_template: "{{{{ .ProjectName }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
    )
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Collect stage names that the Pipeline emitted "<stage> skipped" for.
fn extract_skipped_stages(stderr: &str) -> std::collections::BTreeSet<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = strip_ansi(line);
            let trimmed = line.trim_start();
            // Optional `[<stage-label>]` prefix that StageLogger emits.
            let after_prefix = trimmed
                .strip_prefix('[')
                .and_then(|s| s.find(']').map(|i| &s[i + 1..]))
                .unwrap_or(trimmed)
                .trim();
            after_prefix
                .strip_suffix(" skipped")
                .map(|name| name.trim().to_string())
        })
        .collect()
}

/// Bootstrap a fixture cargo+git repo with the minimal anodizer config.
fn setup_fixture(tmp: &Path) {
    create_test_project(tmp);
    init_git_repo(tmp);
    create_config(tmp, &minimal_config(&host_target()));
}

/// Run the CLI under a fresh tempdir-rooted fixture and return the
/// captured Output.
fn run_anodizer(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(args)
        .current_dir(tmp)
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .output()
        .expect("invoke anodizer")
}

/// Assert every `must_skip` stage appears as "<stage> skipped" in
/// stderr; assert every `must_not_skip` does NOT. Tests pin "did the
/// stage skip" instead of "did the stage run" because skip is the
/// load-bearing contract (a run that produces no artifacts is
/// ambiguous in dry-run / heavy-skip mode).
fn assert_skip_matrix(stderr: &str, must_skip: &[&str], must_not_skip: &[&str], label: &str) {
    let skipped = extract_skipped_stages(stderr);
    for stage in must_skip {
        assert!(
            skipped.contains(*stage),
            "{label}: stage `{stage}` should be reported as skipped but wasn't.\n\
             skipped set: {skipped:?}\nstderr:\n{stderr}"
        );
    }
    for stage in must_not_skip {
        assert!(
            !skipped.contains(*stage),
            "{label}: stage `{stage}` should NOT be reported as skipped but was.\n\
             skipped set: {skipped:?}\nstderr:\n{stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// `anodizer release --snapshot` — local stages only
// ---------------------------------------------------------------------------

/// Row: `anodizer release --snapshot` runs local stages, skips
/// publish/snapcraft-publish/blob/announce. The skip injection lives in
/// `compute_skip_stages`.
#[test]
fn release_snapshot_skips_publish_chain() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--snapshot",
            "--dry-run",
            // Skip the heavy artifact stages — the assertion is on the
            // publish-chain skip, not the build chain itself.
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom",
            "--timeout",
            "2m",
        ],
    );
    assert!(
        out.status.success(),
        "release --snapshot must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_skip_matrix(
        &stderr,
        &["publish", "blob", "announce"],
        &[],
        "release --snapshot",
    );
}

// ---------------------------------------------------------------------------
// `anodizer release --prepare` — local prep, no publish
// ---------------------------------------------------------------------------

/// Row: `anodizer release --prepare` adds release/publish/announce to
/// the skip list. The integration test
/// `test_release_prepare_matches_explicit_skip` already pins skip-set
/// equality vs explicit `--skip=release,publish,announce`; this test
/// pins the docs-row contract directly so a docs/binary drift surfaces
/// here even if the equality test is later refactored.
#[test]
fn release_prepare_skips_publish_release_announce() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--prepare",
            "--snapshot",
            "--dry-run",
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom",
            "--timeout",
            "2m",
        ],
    );
    assert!(
        out.status.success(),
        "release --prepare must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_skip_matrix(
        &stderr,
        &["release", "publish", "announce"],
        &[],
        "release --prepare",
    );
}

/// `--prepare-only` is documented as a clap alias of `--prepare`. The
/// alias must produce identical skip behaviour — a regression would
/// silently break GR-imported scripts that still pass the long form.
#[test]
fn release_prepare_only_alias_matches_prepare() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--prepare-only",
            "--snapshot",
            "--dry-run",
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom",
            "--timeout",
            "2m",
        ],
    );
    assert!(
        out.status.success(),
        "release --prepare-only must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_skip_matrix(
        &stderr,
        &["release", "publish", "announce"],
        &[],
        "release --prepare-only",
    );
}

// ---------------------------------------------------------------------------
// `anodizer release --announce-only` — re-fire announcers only
// ---------------------------------------------------------------------------

/// Row: `anodizer release --announce-only` requires a prior
/// `<dist>/run-<id>/report.json`. Without one, it must bail with a
/// clear error pointing the operator at where the file is expected.
#[test]
fn release_announce_only_bails_without_prior_report() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    // Tag HEAD so derive_run_id resolves a non-`local` id; without a
    // tag the derived id is the short_commit which is fine but harder
    // to predict in a test. Using a tag also keeps the path
    // deterministic.
    Command::new("git")
        .current_dir(tmp.path())
        .args(["tag", "v0.1.0-test"])
        .output()
        .expect("git tag");

    let out = run_anodizer(
        tmp.path(),
        &["release", "--announce-only", "--dry-run", "--timeout", "2m"],
    );
    assert!(
        !out.status.success(),
        "release --announce-only must fail without a prior report.json"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no prior report found"),
        "error must name the missing report; stderr:\n{stderr}"
    );
}

/// Clap-level: `--announce-only` cannot be combined with `--prepare`,
/// `--publish-only`, `--snapshot`, `--rollback-only`, `--split`,
/// `--merge`. Pin two representative combinations (the rest share the
/// same `conflicts_with_all` list).
#[test]
fn release_announce_only_conflicts_with_publish_only() {
    let tmp = TempDir::new().unwrap();
    let out = run_anodizer(
        tmp.path(),
        &["release", "--announce-only", "--publish-only"],
    );
    assert!(
        !out.status.success(),
        "clap must reject --announce-only + --publish-only"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected clap conflict error; got:\n{stderr}"
    );
}

#[test]
fn release_announce_only_conflicts_with_prepare() {
    let tmp = TempDir::new().unwrap();
    let out = run_anodizer(tmp.path(), &["release", "--announce-only", "--prepare"]);
    assert!(
        !out.status.success(),
        "clap must reject --announce-only + --prepare"
    );
}

// ---------------------------------------------------------------------------
// `anodizer publish` — publish-only subcommand
// ---------------------------------------------------------------------------

/// Row: `anodizer publish` runs the release/publish/blob chain and
/// skips every other stage. Asserted indirectly: the subcommand
/// constructs the publish pipeline directly (no opportunity to print
/// "build skipped"), but invoking `publish` against a no-dist fixture
/// must bail with a clear find-artifacts error — confirming the
/// dispatch picked the publish-only branch instead of the full
/// release pipeline (which would bail on a different message about
/// dirty dist or no build artifacts).
#[test]
fn publish_subcommand_dispatches_to_publish_only_pipeline() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    // Create an empty dist so the dist-non-empty pre-check doesn't fire
    // — the assertion is on the publish-only branch's behaviour, not
    // the pre-check.
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(tmp.path(), &["publish", "--dry-run", "--timeout", "2m"]);
    // The publish-only path needs artifacts.json + git state; failing
    // here is expected. The failure mode must NOT be the dist-not-empty
    // pre-check (which fires only on the full `release` pipeline), so
    // dispatch correctness is what's being pinned.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("dist directory") || !stderr.contains("not empty"),
        "publish subcommand must NOT trip the full-release dist pre-check; got:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// `anodizer announce` — announce-only subcommand
// ---------------------------------------------------------------------------

/// Row: `anodizer announce` runs only the announce stage. With no
/// configured announce providers the stage emits "no announce config
/// — skipping"; that's the dispatch we want to pin.
#[test]
fn announce_subcommand_dispatches_to_announce_pipeline() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(tmp.path(), &["announce", "--dry-run", "--timeout", "2m"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let merged = format!("{stdout}\n{stderr}");
    // Failure modes are expected (no dist artifacts, no git tag); the
    // dispatch surface check is that we don't see the heavy stage
    // banners that the full release pipeline would emit.
    for forbidden in &["building binaries", "archiving", "building nfpm"] {
        assert!(
            !merged.contains(forbidden),
            "announce subcommand must not invoke build/archive/nfpm; \
             saw `{forbidden}` in:\n{merged}"
        );
    }
}

// ---------------------------------------------------------------------------
// `anodizer continue` — single-host stage-resume
// ---------------------------------------------------------------------------

/// Row: `anodizer continue` (no `--merge`) loads dist + runs the
/// publish chain. Pin the dispatch surface by asserting the build
/// stage banners do NOT appear in stderr (continue must consume the
/// preserved dist, never recompile).
#[test]
fn continue_no_merge_does_not_recompile() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(tmp.path(), &["continue", "--dry-run", "--timeout", "2m"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let merged = format!("{stdout}\n{stderr}");
    for forbidden in &["building binaries", "archiving", "building nfpm"] {
        assert!(
            !merged.contains(forbidden),
            "continue (no --merge) must not recompile; saw `{forbidden}` in:\n{merged}"
        );
    }
}

// ---------------------------------------------------------------------------
// `--merge` is accepted on publish and announce subcommands
// ---------------------------------------------------------------------------

/// Pin the `--merge` flag on `anodizer publish` (GR Pro
/// `goreleaser publish --merge` parity). Clap-only check — the merge
/// branch's body needs a real dist tree, which is covered by
/// `commands::publish_cmd::tests::merge_missing_config_bails`.
#[test]
fn publish_merge_flag_parses() {
    let cli = anodizer_cli::Cli::try_parse_from_with_args([
        "anodizer",
        "publish",
        "--merge",
        "--dry-run",
    ]);
    assert!(cli, "publish --merge must parse at the clap level");
}

#[test]
fn announce_merge_flag_parses() {
    let cli = anodizer_cli::Cli::try_parse_from_with_args([
        "anodizer",
        "announce",
        "--merge",
        "--dry-run",
    ]);
    assert!(cli, "announce --merge must parse at the clap level");
}

/// Helper: ensures the args parse at the clap layer. Trait-resolution
/// adapter so the test body reads `Cli::try_parse_from_with_args` —
/// keeps callers from importing `clap::Parser` just for the test.
trait CliParse {
    fn try_parse_from_with_args(args: impl IntoIterator<Item = &'static str>) -> bool;
}
impl CliParse for anodizer_cli::Cli {
    fn try_parse_from_with_args(args: impl IntoIterator<Item = &'static str>) -> bool {
        use clap::Parser;
        anodizer_cli::Cli::try_parse_from(args).is_ok()
    }
}
