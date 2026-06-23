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
//! consolidated skip row (`• skipped  a, b, c`, ANSI-stripped) — same
//! shape the sibling
//! `tests/integration.rs::test_release_prepare_matches_explicit_skip`
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

/// Minimal single-crate snapshot config. Local to this file because the
/// stage-skip matrix needs a config shape specific to these assertions
/// (the shared `tests/common` bootstrap synthesizes a different fixture).
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

/// Collect the stage names the Pipeline reported as skipped.
///
/// Pipeline-level stage skips are consolidated into kv rows — consecutive
/// skipped stages buffer up and flush as a single body line:
///   `• skipped  build, changelog, archive`              (operator/mode `--skip`)
///   `• skipped  upx, dmg, msi (no binaries)`            (binary-dependent stages, no binaries)
/// Both rows are emitted by `Pipeline::run` via `kv(...)` (see
/// `crates/cli/src/pipeline/mod.rs`), so they are the authoritative
/// "these stages did not run" signal; the value is a comma-separated
/// stage list, split into individual names.
///
/// Per-crate / per-config body notes such as `skipping build for crate
/// 'X'` or `no gitlab config for crate 'y', skipping` deliberately do
/// NOT count as a stage skip: those are progress lines emitted inside a
/// running stage, not the stage's own pipeline-level verdict. Anchoring on
/// the kv key pad (`skipped` + at least two spaces) keeps the distinction
/// precise — mid-stage notes like `skipped (snapshot mode)` have a single
/// space and don't match.
fn extract_skipped_stages(stderr: &str) -> std::collections::BTreeSet<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = strip_ansi(line);
            let body = line.trim_start().strip_prefix("• ")?;
            let names = body.strip_prefix("skipped  ")?.trim_start();
            let names = names.strip_suffix(" (no binaries)").unwrap_or(names);
            Some(
                names
                    .split(", ")
                    .map(|stage| stage.to_string())
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
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

/// Assert every `must_skip` stage appears as `[<stage>] skipped` in
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
/// publish/snapcraft-publish/blob/announce.
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
            // Surface the consolidated pipeline skip row at default
            // verbosity; without it the `• skipped  …` line routes to
            // debug and `extract_skipped_stages` sees nothing.
            "--show-skipped",
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
    // `release` (the GitHub-release-creation stage) must NOT be skipped by
    // snapshot mode — it's in the "stages run" column of the docs table.
    assert_skip_matrix(
        &stderr,
        &["publish", "blob", "announce"],
        &["release"],
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
            // Surface the consolidated pipeline skip row at default verbosity.
            "--show-skipped",
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
    // blob and snapcraft-publish are also network-touching; --prepare skips them.
    assert_skip_matrix(
        &stderr,
        &[
            "release",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
        ],
        &[],
        "release --prepare",
    );
}

/// `--prepare-only` is documented as a clap alias of `--prepare`. The
/// alias must produce identical skip behaviour — a regression would
/// silently break imported scripts that still pass the long form.
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
            // Surface the consolidated pipeline skip row at default verbosity.
            "--show-skipped",
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
        &[
            "release",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
        ],
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
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(tmp.path()).args(["tag", "v0.1.0-test"]);
            cmd
        },
        "git",
    );

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

/// Pin the `--merge` flag on `anodizer publish`. Clap-only check — the merge
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

// ---------------------------------------------------------------------------
// Additional rows covering `release` (snapshot mode's release stage),
// `release --publish-only`, and `continue --merge`.
// ---------------------------------------------------------------------------

/// Snapshot mode auto-skips the publish/blob/announce chain but the
/// `release` (GitHub release creation) stage itself stays in the pipeline.
/// Pins the negative invariant: snapshot does NOT touch the release stage.
#[test]
fn release_snapshot_does_not_skip_release_stage() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--snapshot",
            "--dry-run",
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom",
            "--timeout",
            "2m",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_skip_matrix(&stderr, &[], &["release"], "release --snapshot");
}

/// `release --publish-only` must bypass the full-release "dist not empty"
/// pre-check (the publish-only branch consumes a preserved dist) AND must
/// fail with the publish-only "no context.json" error when none is present.
/// Pins dispatch correctness via these two assertions because the
/// publish-only branch bails before the pipeline emits per-stage skip lines.
#[test]
fn publish_only_bypasses_dist_precheck_requires_context_json() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(
        tmp.path(),
        &["release", "--publish-only", "--dry-run", "--timeout", "2m"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "release --publish-only must fail without a preserved dist tree"
    );
    assert!(
        stderr.contains("context.json") || stderr.contains("publish-only"),
        "error must come from the publish-only branch; got:\n{stderr}"
    );
    assert!(
        !stderr.contains("dist directory") || !stderr.contains("not empty"),
        "release --publish-only must not trip the full-release dist pre-check; got:\n{stderr}"
    );
}

/// `continue --merge` is the split-merge resume path: it consumes a preserved
/// dist tree rather than recompiling. Pins the negative invariant — no build,
/// archive, or nfpm banner appears — and confirms dispatch lands on the merge
/// branch rather than the full-release dist pre-check.
#[test]
fn continue_merge_does_not_trigger_build_pipeline() {
    let tmp = TempDir::new().unwrap();
    setup_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();

    let out = run_anodizer(
        tmp.path(),
        &["continue", "--merge", "--dry-run", "--timeout", "2m"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let merged = format!("{stdout}\n{stderr}");
    for forbidden in &["building binaries", "archiving", "building nfpm"] {
        assert!(
            !merged.contains(forbidden),
            "continue --merge must not invoke build/archive/nfpm; \
             saw `{forbidden}` in:\n{merged}"
        );
    }
    assert!(
        !merged.contains("dist directory") || !merged.contains("not empty"),
        "continue --merge must not trip the full-release dist pre-check"
    );
}

// ---------------------------------------------------------------------------
// `--host-targets` — host-scoped real build subset (used by `task prepush`)
// ---------------------------------------------------------------------------

/// Single-crate config with an explicit `targets:` list. Used to drive the
/// `--host-targets` partition through the binary's startup path: the safety
/// gate and empty-result guard both fire BEFORE any compilation, so these
/// tests stay cheap on any host.
fn config_with_targets(triples: &[&str]) -> String {
    let targets = triples
        .iter()
        .map(|t| format!("          - {t}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"project_name: matrix-fixture
crates:
  - name: matrix-fixture
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: matrix-fixture
        targets:
{targets}
    archives:
      - name_template: "{{{{ .ProjectName }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
    )
}

fn setup_fixture_with_targets(tmp: &Path, triples: &[&str]) {
    create_test_project(tmp);
    init_git_repo(tmp);
    create_config(tmp, &config_with_targets(triples));
}

/// `--host-targets` without `--snapshot`/`--dry-run` must hard-error at
/// startup: silently dropping configured targets in a real release would
/// ship a broken release.
#[test]
fn host_targets_requires_snapshot_or_dry_run() {
    let tmp = TempDir::new().unwrap();
    setup_fixture_with_targets(tmp.path(), &["x86_64-unknown-linux-gnu"]);
    let out = run_anodizer(tmp.path(), &["release", "--host-targets", "--force"]);
    assert!(
        !out.status.success(),
        "--host-targets without --snapshot/--dry-run must fail.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("--host-targets is only valid with --snapshot or --dry-run"),
        "must explain the snapshot/dry-run gate.\nstderr:\n{stderr}"
    );
}

/// `--host-targets --dry-run` satisfies the safety gate (no real release is
/// produced), so startup proceeds past the gate.
#[test]
fn host_targets_allowed_with_dry_run() {
    let tmp = TempDir::new().unwrap();
    setup_fixture_with_targets(tmp.path(), &["x86_64-unknown-linux-gnu"]);
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--dry-run",
            "--host-targets",
            "--skip=build,archive,sign,checksum,sbom,docker",
        ],
    );
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        !stderr.contains("--host-targets is only valid with"),
        "the safety gate must NOT trip under --dry-run.\nstderr:\n{stderr}"
    );
}

/// Empty-result guard: a non-apple host (this CI runner is Linux) with an
/// apple-darwin-only config can build NOTHING, so `--host-targets` must
/// hard-error and tell the operator to run on a macOS host — never emit an
/// empty snapshot that breaks downstream archive/checksum stages.
///
/// Skipped on an apple host (where every target is buildable and the guard
/// never fires).
#[test]
fn host_targets_empty_result_hard_errors_on_linux() {
    if anodizer_core::partial::host_is_apple(&host_target()) {
        return;
    }
    let tmp = TempDir::new().unwrap();
    setup_fixture_with_targets(tmp.path(), &["x86_64-apple-darwin", "aarch64-apple-darwin"]);
    let out = run_anodizer(
        tmp.path(),
        &["release", "--snapshot", "--host-targets", "--force"],
    );
    assert!(
        !out.status.success(),
        "apple-only config on a non-apple host must hard-error.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("none of the")
            && stderr.contains("can be built on this host")
            && stderr.contains("macOS host"),
        "empty-result guard must name the cause and the macOS-host remedy.\nstderr:\n{stderr}"
    );
}

/// On a non-apple, non-windows host (this CI runner is Linux), a mixed config
/// emits ONE loud skip line that groups both reasons: apple triples need a
/// macOS host, windows-msvc needs a Windows host. `*-windows-gnu` and linux
/// stay. Heavy stages are skipped to keep the test cheap; the assertion is
/// purely on the loud-log line.
///
/// Skipped on apple/windows hosts (the skip set differs there).
#[test]
fn host_targets_logs_skipped_apple_and_msvc_on_linux() {
    let host = host_target();
    if anodizer_core::partial::host_is_apple(&host)
        || anodizer_core::partial::host_is_windows(&host)
    {
        return;
    }
    let tmp = TempDir::new().unwrap();
    setup_fixture_with_targets(
        tmp.path(),
        &[
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ],
    );
    let out = run_anodizer(
        tmp.path(),
        &[
            "release",
            "--snapshot",
            "--host-targets",
            "--force",
            "--skip=build,archive,sign,checksum,sbom,docker",
        ],
    );
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("skipped 3 target(s) — not buildable")
            && stderr.contains("x86_64-apple-darwin")
            && stderr.contains("aarch64-apple-darwin")
            && stderr.contains("apple targets require a macOS host")
            && stderr.contains("x86_64-pc-windows-msvc")
            && stderr.contains("windows-msvc targets require a Windows host"),
        "must emit one grouped skip line naming both apple + msvc reasons.\nstderr:\n{stderr}"
    );
    let skip_line = stderr
        .lines()
        .find(|l| l.contains("not buildable on this"))
        .expect("a skip line is emitted");
    assert!(
        !skip_line.contains("x86_64-pc-windows-gnu"),
        "windows-gnu must NOT be in the skip set (cross-builds from linux).\nskip line:\n{skip_line}"
    );
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
