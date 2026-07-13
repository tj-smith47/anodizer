//! Partial-publish rollback tests.

// ---------------------------------------------------------------------------
// Partial-publish rollback: a multi-crate publish that succeeds on crate A
// then fails on crate B must record A (and only A) so rollback yanks the
// crate that actually went live — even when the local `.crate` files are
// gone. These tests stub `cargo` on PATH so the publish loop and the
// rollback yank loop exercise the real spawn surface without a network
// round-trip.
// ---------------------------------------------------------------------------

use super::*;
use anodizer_core::Publisher;
use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write a crate source dir with a `[package]` manifest pinning
/// `version`, returning the dir path for use as `CrateConfig.path`.
fn write_crate_dir(root: &Path, name: &str, version: &str) -> String {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir crate");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
    )
    .expect("write Cargo.toml");
    dir.display().to_string()
}

/// `git init` + commit everything under `dir`, yielding a CLEAN working
/// tree the cleanliness gate (`ensure_publish_tree_clean`) can verify.
///
/// The guard fails CLOSED when `git status` cannot prove cleanliness (a
/// non-git dir errors), so these fixtures must back their `project_root`
/// with a real repo to exercise the genuine clean-pass rather than the old
/// fail-open hole.
fn init_clean_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(dir)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@example.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@example.com");
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    // Ignore the per-test runtime scratch (cargo-stub binary + its argv
    // log) so the gate sees a CLEAN tree at entry: those files are test
    // harness artifacts, not source, and the stub's argv log is written
    // mid-run AFTER the cleanliness gate has already passed.
    std::fs::write(dir.join(".gitignore"), "cargo\nargv.log\n").expect("write .gitignore");
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture"]);
}

/// Install a `cargo` shell stub on PATH that appends each invocation's
/// argv (one line per call) to `argv_log` and chooses its exit code by
/// argv: a `cargo publish -p <fail_crate>` exits 1; every other call
/// (other publishes, `cargo yank`) exits 0. Returns a PATH value with
/// the stub dir prepended; the caller installs it under a `#[serial]`
/// guard and restores the prior value.
pub(super) fn install_cargo_stub(dir: &Path, argv_log: &Path, fail_crate: &str) -> String {
    let stub = dir.join("cargo");
    let script = format!(
        "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = publish ]; then\n\
             for a in \"$@\"; do\n\
             if [ \"$a\" = '{fail}' ]; then exit 1; fi\n\
             done\n\
             fi\n\
             exit 0\n",
        log = argv_log.display(),
        fail = fail_crate,
    );
    std::fs::write(&stub, script).expect("write cargo stub");
    let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).expect("chmod stub");
    let prev = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", dir.display(), prev)
}

/// Read the stub's recorded argv lines (empty vec when the stub never
/// ran / the log was never created).
pub(super) fn read_argv_log(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Fixed-tag resolver for the guard's binstall pre-publish mutation. These
/// tests use crates with no binstall config (the emitter early-returns), so
/// the resolver is never actually consulted; it exists only to satisfy the
/// `publish_to_cargo_with_guard` signature without a git fixture.
fn fixed_tag_resolver(_ctx: &Context, c: &CrateConfig) -> Option<String> {
    Some(format!("v{}", c.name))
}

/// Fetch closure that panics if invoked — for guard tests whose local
/// cksum either matches the index (fast path) or must never reach the
/// download at all (fail-closed-before-the-guard cases).
fn fetch_panics(
    _: &str,
    _: &str,
    _: &anodizer_core::retry::RetryPolicy,
    _: &StageLogger,
) -> Result<Vec<u8>> {
    panic!("fetch_published must not run on this path")
}

/// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
/// given `(in-tar path, content)` entries — for guard tests that must
/// exercise the slow-path content comparison with real archive bytes.
fn make_crate_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;

    let mut builder = tar::Builder::new(Vec::new());
    for (path, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, *content)
            .expect("append tar entry");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).expect("gzip write");
    gz.finish().expect("gzip finish")
}

/// Minimal `.cargo_vcs_info.json` body: `{"git":{"sha1":"<sha>"}}`.
fn vcs_info_json(sha1: &str) -> Vec<u8> {
    format!(r#"{{"git":{{"sha1":"{sha1}"}}}}"#).into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
}

/// Always-not-published injection: drives the publish loop straight to
/// the `cargo publish` spawn without a sparse-index GET.
fn never_published(
    _name: &str,
    _version: &str,
    _policy: &anodizer_core::retry::RetryPolicy,
    _log: &StageLogger,
) -> Result<Option<String>> {
    Ok(None)
}

/// Index injection used by the wait-gate wiring test: the workspace
/// dependency `dep-crate` is reported already-live on crates.io (so the
/// dep-completeness guard passes — the legitimate multi-tag case), while
/// the crate being published (`leaf`) is reported absent (so the loop's
/// idempotency check does NOT skip it and the wait-gate actually runs).
fn dep_published_leaf_clean(
    name: &str,
    _version: &str,
    _policy: &anodizer_core::retry::RetryPolicy,
    _log: &StageLogger,
) -> Result<Option<String>> {
    if name == "dep-crate" {
        Ok(Some("deadbeef".to_string()))
    } else {
        Ok(None)
    }
}

fn cargo_crate(name: &str, path: &str, deps: &[&str], cfg: CargoPublishConfig) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        publish: Some(PublishConfig {
            cargo: Some(cfg),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// crate-a publishes; crate-b (which depends on a, so a goes first)
/// fails. The success record must contain ONLY crate-a, with its
/// per-crate version and configured registry — never crate-b
/// (publish failed) or any skipped/never-published crate.
#[test]
#[serial(cargo_stub_path)]
fn partial_publish_records_only_succeeded_crate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
    let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
    let argv_log = tmp.path().join("argv.log");

    // crate-a: skip its post-publish index poll (it has a dependent),
    // and pin a registry so the recorded snapshot carries it.
    let cfg_a = CargoPublishConfig {
        index_timeout: Some(0),
        registry: Some("my-registry".to_string()),
        ..Default::default()
    };
    // crate-b depends on crate-a → topological order publishes a first.
    let crate_a = cargo_crate("crate-a", &path_a, &[], cfg_a);
    let crate_b = cargo_crate(
        "crate-b",
        &path_b,
        &["crate-a"],
        CargoPublishConfig::default(),
    );

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_a, crate_b])
        .selected_crates(vec!["crate-b".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
    init_clean_repo(tmp.path());
    let _env = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Read the previous PATH under the lock so a concurrent mutator
    // cannot interleave between the read and the set below.
    let prev_path = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex above (shared with every other
    // PATH mutator) plus this test's serial group; paired restore below.
    // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
    unsafe { std::env::set_var("PATH", &new_path) };
    let result = publish_to_cargo_with(
        &mut ctx,
        &["crate-b".to_string()],
        &log,
        &mut record,
        never_published,
        None,
    );
    // SAFETY: restore PATH within the same serial group.
    unsafe {
        match prev_path {
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            None => std::env::remove_var("PATH"),
        }
    }

    assert!(result.is_err(), "crate-b's publish failure must surface");

    // The stub must have seen BOTH publishes (a succeeds, b fails).
    let argv = read_argv_log(&argv_log);
    assert!(
        argv.iter()
            .any(|l| l.contains("publish") && l.contains("crate-a")),
        "stub should have run crate-a's publish: {argv:?}"
    );
    assert!(
        argv.iter()
            .any(|l| l.contains("publish") && l.contains("crate-b")),
        "stub should have run crate-b's publish: {argv:?}"
    );

    // Record holds crate-a only, with its version + registry.
    assert_eq!(
        record.len(),
        1,
        "only the succeeded crate is recorded: {record:?}"
    );
    let rec = &record[0];
    assert_eq!(rec.name, "crate-a");
    assert_eq!(rec.version, "1.0.0");
    assert_eq!(rec.registry.as_deref(), Some("my-registry"));
    assert!(rec.index.is_none());
}

/// End-to-end through the Publisher trait: the failed `run` stashes the
/// partial evidence on the context (crate-a only); `rollback` reads it
/// and issues exactly one `cargo yank` — for crate-a, on its configured
/// registry — and never touches crate-b (never published).
#[test]
#[serial(cargo_stub_path)]
fn run_failure_then_rollback_yanks_only_succeeded_crate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
    let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
    let argv_log = tmp.path().join("argv.log");

    let cfg_a = CargoPublishConfig {
        index_timeout: Some(0),
        registry: Some("my-registry".to_string()),
        ..Default::default()
    };
    let crate_a = cargo_crate("crate-a", &path_a, &[], cfg_a);
    let crate_b = cargo_crate(
        "crate-b",
        &path_b,
        &["crate-a"],
        CargoPublishConfig::default(),
    );

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_a, crate_b])
        .selected_crates(vec!["crate-b".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    // Build the evidence the failed publish would record, exactly as
    // `CargoPublisher::run` does, by driving the injected publish loop
    // and encoding whatever it recorded before the bail.
    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
    init_clean_repo(tmp.path());
    let _env = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Read the previous PATH under the lock so a concurrent mutator
    // cannot interleave between the read and the set below.
    let prev_path = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex above (shared with every other
    // PATH mutator) plus this test's serial group; paired restore below.
    // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
    unsafe { std::env::set_var("PATH", &new_path) };

    let mut record: Vec<CargoYankTarget> = Vec::new();
    let publish_result = publish_to_cargo_with(
        &mut ctx,
        &["crate-b".to_string()],
        &log,
        &mut record,
        never_published,
        None,
    );
    assert!(publish_result.is_err(), "crate-b failure surfaces");

    let mut evidence = anodizer_core::PublishEvidence::new("cargo");
    evidence.extra = encode_cargo_yank_targets(&record);

    // Wipe the publish argv before rollback so we assert only on the
    // yank invocations the rollback issues.
    std::fs::write(&argv_log, b"").expect("truncate argv log");

    let publisher = CargoPublisher::new();
    let rb = publisher.rollback(&mut ctx, &evidence);

    // SAFETY: restore PATH within the same serial group.
    unsafe {
        match prev_path {
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            None => std::env::remove_var("PATH"),
        }
    }
    rb.expect("rollback ok");

    let yanks: Vec<String> = read_argv_log(&argv_log)
        .into_iter()
        .filter(|l| l.starts_with("yank"))
        .collect();
    assert_eq!(yanks.len(), 1, "exactly one crate is yanked: {yanks:?}");
    let line = &yanks[0];
    assert!(
        line.contains("--version 1.0.0"),
        "yank carries the version: {line}"
    );
    assert!(line.contains("crate-a"), "yank targets crate-a: {line}");
    assert!(
        line.contains("--registry my-registry"),
        "yank targets the registry: {line}"
    );
    assert!(
        !line.contains("crate-b"),
        "crate-b was never published; must not be yanked: {line}"
    );
}

/// Empty record (the publisher failed before its first successful
/// publish, or nothing was eligible): rollback is a clean no-op — it
/// spawns no `cargo` and returns Ok, rather than emitting a scary warn.
#[test]
#[serial(cargo_stub_path)]
fn rollback_is_clean_noop_when_nothing_published() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let argv_log = tmp.path().join("argv.log");

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .project_root(tmp.path().to_path_buf())
        .build();
    let mut evidence = anodizer_core::PublishEvidence::new("cargo");
    evidence.extra = encode_cargo_yank_targets(&[]);

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
    init_clean_repo(tmp.path());
    let _env = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Read the previous PATH under the lock so a concurrent mutator
    // cannot interleave between the read and the set below.
    let prev_path = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex above (shared with every other
    // PATH mutator) plus this test's serial group; paired restore below.
    // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
    unsafe { std::env::set_var("PATH", &new_path) };

    let publisher = CargoPublisher::new();
    let rb = publisher.rollback(&mut ctx, &evidence);

    // SAFETY: restore PATH within the same serial group.
    unsafe {
        match prev_path {
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            None => std::env::remove_var("PATH"),
        }
    }
    rb.expect("rollback no-op ok");

    assert!(
        read_argv_log(&argv_log).is_empty(),
        "no-op rollback must not spawn cargo"
    );
}

/// Install a `cargo` stub that records argv and exits non-zero for
/// `cargo yank` (every other call exits 0). Drives the rollback
/// yank-failure branch so the `failed` counter + warn path are exercised.
fn install_yank_failing_stub(dir: &Path, argv_log: &Path) -> String {
    let stub = dir.join("cargo");
    let script = format!(
        "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = yank ]; then\n\
             echo 'error: api errored: 403 forbidden' >&2\n\
             exit 1\n\
             fi\n\
             exit 0\n",
        log = argv_log.display(),
    );
    std::fs::write(&stub, script).expect("write cargo stub");
    let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).expect("chmod stub");
    let prev = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", dir.display(), prev)
}

/// Run `f` with `PATH` prepended to `new_path` under the serial guard,
/// restoring the previous value afterward. Keeps the set/restore pairing
/// out of each test body.
fn with_path<R>(new_path: &str, f: impl FnOnce() -> R) -> R {
    let _env = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex above (shared with every other
    // PATH mutator in the workspace, including fake_tool::activate)
    // plus the callers' `#[serial(cargo_stub_path)]` guard; paired
    // restore below.
    // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
    unsafe { std::env::set_var("PATH", new_path) };
    let out = f();
    // SAFETY: restore the prior PATH (paired with the set above).
    unsafe {
        match prev {
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            None => std::env::remove_var("PATH"),
        }
    }
    out
}

/// Rollback whose `cargo yank` fails: the publisher must NOT propagate
/// the error (rollback is best-effort), still record the failure, and
/// emit the per-target warn. We assert the yank was attempted with the
/// recorded version and that rollback returns Ok despite the non-zero
/// exit.
#[test]
#[serial(cargo_stub_path)]
fn rollback_continues_and_warns_when_yank_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let argv_log = tmp.path().join("argv.log");

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .project_root(tmp.path().to_path_buf())
        .build();
    let mut evidence = anodizer_core::PublishEvidence::new("cargo");
    evidence.extra = encode_cargo_yank_targets(&[CargoYankTarget {
        name: "crate-x".into(),
        version: "1.4.2".into(),
        registry: None,
        index: None,
    }]);

    let new_path = install_yank_failing_stub(tmp.path(), &argv_log);
    let publisher = CargoPublisher::new();
    let rb = with_path(&new_path, || publisher.rollback(&mut ctx, &evidence));
    // Best-effort: a failed yank must NOT turn rollback into an Err.
    rb.expect("rollback tolerates a failed yank");

    let yanks: Vec<String> = read_argv_log(&argv_log)
        .into_iter()
        .filter(|l| l.starts_with("yank"))
        .collect();
    assert_eq!(
        yanks.len(),
        1,
        "the single target is yanked once: {yanks:?}"
    );
    assert!(
        yanks[0].contains("--version 1.4.2") && yanks[0].contains("crate-x"),
        "yank carries the recorded version + name: {}",
        yanks[0]
    );
}

/// A recorded target with an `index` (not a `registry`) threads
/// `--index <url>` into the yank argv. Pins the index-arg branch of the
/// rollback yank command builder.
#[test]
#[serial(cargo_stub_path)]
fn rollback_yank_threads_index_arg() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let argv_log = tmp.path().join("argv.log");

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .project_root(tmp.path().to_path_buf())
        .build();
    let mut evidence = anodizer_core::PublishEvidence::new("cargo");
    evidence.extra = encode_cargo_yank_targets(&[CargoYankTarget {
        name: "crate-idx".into(),
        version: "0.2.0".into(),
        registry: None,
        index: Some("sparse+https://example.test/index/".into()),
    }]);

    // `none` never matches a publish arg, so this stub exits 0 for yank.
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
    init_clean_repo(tmp.path());
    let publisher = CargoPublisher::new();
    with_path(&new_path, || publisher.rollback(&mut ctx, &evidence)).expect("rollback ok");

    let yank = read_argv_log(&argv_log)
        .into_iter()
        .find(|l| l.starts_with("yank"))
        .expect("a yank was issued");
    assert!(
        yank.contains("--index sparse+https://example.test/index/"),
        "index target must thread --index: {yank}"
    );
    assert!(
        !yank.contains("--registry"),
        "index-only target must NOT carry --registry: {yank}"
    );
}

/// A crate whose resolved version is empty (no `[package].version` on
/// disk AND a blank release version) is published but CANNOT be recorded
/// for auto-yank: the loop emits the "CANNOT be auto-yanked" warn and the
/// success record stays empty, so a later failure leaves nothing to yank.
#[test]
#[serial(cargo_stub_path)]
fn empty_version_publish_is_not_recorded_for_yank() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Manifest with NO version field ⇒ read_cargo_toml_version → None.
    let dir = tmp.path().join("noversion");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"noversion\"\n")
        .expect("write manifest");
    let argv_log = tmp.path().join("argv.log");

    let crate_nv = cargo_crate(
        "noversion",
        &dir.display().to_string(),
        &[],
        CargoPublishConfig::default(),
    );
    // Suppress git-var population so the release-version fallback is also
    // empty — without this the builder's default semver (1.2.3) fills in.
    let mut ctx = TestContextBuilder::new()
        .populate_git_vars(false)
        .crates(vec![crate_nv])
        .selected_crates(vec!["noversion".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();
    // never_published would early-skip on a non-empty version, but the
    // empty-version branch bypasses the index check entirely and goes
    // straight to publish — so the stub's `cargo publish` runs.
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail-crate");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with(
            &mut ctx,
            &["noversion".to_string()],
            &log,
            &mut record,
            never_published,
            None,
        )
    });
    result.expect("publish of a version-less crate still succeeds");

    // The publish ran...
    assert!(
        read_argv_log(&argv_log)
            .iter()
            .any(|l| l.contains("publish") && l.contains("noversion")),
        "version-less crate is still published"
    );
    // ...but NOTHING is recorded, because an empty version can't be yanked.
    assert!(
        record.is_empty(),
        "empty-version publish must NOT be recorded for auto-yank: {record:?}"
    );
}

/// Already-published idempotency: when the index reports the version live
/// (`Ok(Some(cksum)`) AND the local `.crate` is byte-identical, the publish
/// loop SKIPS that crate — `cargo publish` is never spawned and nothing is
/// recorded. The content-vs-version guard only treats a match as a safe
/// skip; the identical-content path is the legitimate idempotent re-cut.
#[test]
#[serial(cargo_stub_path)]
fn already_published_crate_is_skipped_not_republished() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "live-crate", "9.9.9");
    let argv_log = tmp.path().join("argv.log");

    let crate_cfg = cargo_crate("live-crate", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v9.9.9")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["live-crate".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    // Inject "already on crates.io with this cksum" for every query, and a
    // local `.crate` checksum that MATCHES — the safe idempotent re-cut.
    let always_published = |_n: &str,
                            _v: &str,
                            _p: &anodizer_core::retry::RetryPolicy,
                            _l: &StageLogger|
     -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };
    let local_matches = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "deadbeef".to_string(),
            bytes: Vec::new(),
        }))
    };

    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["live-crate".to_string()],
            &log,
            &mut record,
            always_published,
            local_matches,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    result.expect("already-published-identical path returns Ok");

    assert!(
        read_argv_log(&argv_log).is_empty(),
        "already-published crate must NOT spawn cargo publish"
    );
    assert!(
        record.is_empty(),
        "a skipped (already-published) crate is not recorded for yank"
    );
}

/// Index-check error (`Err`) for a never-published crate (`crate_version`
/// resolves but the index is unreachable) FAILS CLOSED: the loop refuses
/// to skip a version it cannot confirm is byte-identical to the published
/// artifact, because silently skipping a possibly-poisoned version is the
/// exact failure the content-vs-version guard prevents.
#[test]
#[serial(cargo_stub_path)]
fn index_check_error_fails_closed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "flaky", "1.0.0");
    let argv_log = tmp.path().join("argv.log");

    let crate_cfg = cargo_crate("flaky", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["flaky".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    let index_errors =
        |_n: &str,
         _v: &str,
         _p: &anodizer_core::retry::RetryPolicy,
         _l: &StageLogger|
         -> Result<Option<String>> { Err(anyhow::anyhow!("index transport blew up")) };
    let local_unused = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "unused".to_string(),
            bytes: Vec::new(),
        }))
    };

    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["flaky".to_string()],
            &log,
            &mut record,
            index_errors,
            local_unused,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    let err = result.expect_err("index error must fail closed, not publish blindly");
    assert!(
        format!("{err:#}").contains("could not reach the crates.io index"),
        "fail-closed error names the network cause: {err:#}"
    );
    assert!(
        read_argv_log(&argv_log).is_empty(),
        "must NOT publish when the skip decision is unverifiable"
    );
    assert!(record.is_empty(), "nothing published ⇒ nothing recorded");
}

/// `wait_for_workspace_deps` integration: when enabled and the crate has
/// a literal-pinned workspace dep, the loop polls crates.io for that dep.
/// We point the dep's expected version at one already on a local index
/// responder so the gate clears in one probe — proving the gate is wired
/// into the publish loop (not just unit-tested in isolation). The dep
/// pin uses a crate name whose sparse-index URL we can serve locally is
/// impossible (the gate computes the real index URL), so instead we set
/// a tiny max_wait and assert the gate's TIMEOUT error surfaces through
/// the publish loop's context — proving the wiring fires.
#[test]
#[serial(cargo_stub_path)]
fn wait_for_workspace_deps_gate_is_wired_into_publish_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Leaf with a literal-pinned workspace-internal dep that will never
    // appear (bogus version on the real index) → the gate times out.
    let dir = tmp.path().join("leaf");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"leaf\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\ndep-crate = { path = \"../dep\", version = \"0.0.0-never-exists\" }\n",
    )
    .expect("write manifest");
    let argv_log = tmp.path().join("argv.log");

    use anodizer_core::config::HumanDuration;
    use std::time::Duration;
    let wait_cfg = WaitForWorkspaceDepsConfig {
        enabled: Some(true),
        // Sub-millisecond budget so the timeout fires fast.
        max_wait: Some(HumanDuration(Duration::from_millis(1))),
        poll_interval: Some(HumanDuration(Duration::from_millis(1))),
    };
    let leaf = cargo_crate(
        "leaf",
        &dir.display().to_string(),
        &["dep-crate"],
        CargoPublishConfig {
            wait_for_workspace_deps: Some(wait_cfg),
            ..Default::default()
        },
    );
    // `dep-crate` is in the config (so it counts as workspace-internal)
    // but has no cargo block, so it isn't itself published.
    let dep = CrateConfig {
        name: "dep-crate".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![leaf, dep])
        .selected_crates(vec!["leaf".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
    init_clean_repo(tmp.path());
    // The dep-completeness guard runs first; inject `always_published` so
    // it treats `dep-crate` as live on crates.io (the legitimate multi-tag
    // case the wait-gate is for) and the wait-gate TIMEOUT — not the guard
    // — is the failure under test. The wait-gate itself polls the REAL
    // index for the bogus `0.0.0-never-exists` version, so it still times
    // out as intended.
    let result = with_path(&new_path, || {
        publish_to_cargo_with(
            &mut ctx,
            &["leaf".to_string()],
            &log,
            &mut record,
            dep_published_leaf_clean,
            None,
        )
    });
    let err = result.expect_err("wait_for_workspace_deps timeout must surface");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("wait_for_workspace_deps"),
        "the gate error must be threaded through the publish loop: {chain}"
    );
    // The gate fired BEFORE the publish spawn, so cargo was never run.
    assert!(
        read_argv_log(&argv_log).is_empty(),
        "publish must not spawn while the dep gate is still blocking"
    );
}

/// End-to-end through `CargoPublisher::run`: a multi-crate publish that
/// fails on the second crate stashes the partial evidence on the context
/// (the Err arm of `run`) so the dispatcher can recover it for rollback.
/// Asserts the stashed evidence records ONLY the first (succeeded) crate.
#[test]
#[serial(cargo_stub_path)]
fn run_failure_stashes_partial_evidence_on_context() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
    let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
    let argv_log = tmp.path().join("argv.log");

    let crate_a = cargo_crate(
        "crate-a",
        &path_a,
        &[],
        CargoPublishConfig {
            index_timeout: Some(0),
            ..Default::default()
        },
    );
    let crate_b = cargo_crate(
        "crate-b",
        &path_b,
        &["crate-a"],
        CargoPublishConfig::default(),
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_a, crate_b])
        .selected_crates(vec!["crate-b".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
    init_clean_repo(tmp.path());
    let publisher = CargoPublisher::new();
    let run_result = with_path(&new_path, || publisher.run(&mut ctx));
    assert!(run_result.is_err(), "crate-b failure surfaces from run");

    // The Err arm recorded the partial evidence on the context.
    let pending = ctx
        .take_pending_evidence()
        .expect("failed run must stash pending evidence for rollback");
    let targets = decode_cargo_yank_targets(&pending.extra);
    assert_eq!(targets.len(), 1, "only crate-a is recorded: {targets:?}");
    assert_eq!(targets[0].name, "crate-a");
    assert_eq!(targets[0].version, "1.0.0");
}

/// When a crate's Cargo.toml has no resolvable version, the skip-decision
/// must treat it as "not yet published" (attempt publish) — NOT key the
/// idempotency probe on the global release version.
///
/// The old code used `unwrap_or_else(|| release_version.clone())` which
/// caused `already_published_check("my-crate", "1.0.0")` to return
/// `Some(cksum)` → the crate was silently skipped even though its real
/// version had never been published.
#[test]
#[serial(cargo_stub_path)]
fn manifest_read_failure_does_not_skip_publish() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Write a Cargo.toml WITHOUT a version field — simulates the case
    // where `read_cargo_toml_version` returns None.
    let crate_dir = tmp.path().join("my-crate");
    std::fs::create_dir_all(&crate_dir).expect("mkdir");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"my-crate\"\n# no version field\n",
    )
    .expect("write Cargo.toml");
    let argv_log = tmp.path().join("argv.log");

    let crate_cfg = cargo_crate(
        "my-crate",
        &crate_dir.display().to_string(),
        &[],
        CargoPublishConfig {
            index_timeout: Some(0),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_cfg])
        .project_root(tmp.path().to_path_buf())
        .build();

    // The "1.0.0" release version IS already on crates.io — if we
    // incorrectly keyed the skip-decision on it, the crate would be
    // skipped. The correct behaviour is to attempt publish anyway because
    // the per-crate version is unresolvable.
    let always_published_1_0_0 =
        |_name: &str,
         _version: &str,
         _policy: &anodizer_core::retry::RetryPolicy,
         _l: &StageLogger|
         -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
    init_clean_repo(tmp.path());
    let _env = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Read the previous PATH under the lock so a concurrent mutator
    // cannot interleave between the read and the set below.
    let prev_path = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex above (shared with every other
    // PATH mutator) plus this test's serial group; paired restore below.
    // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
    unsafe { std::env::set_var("PATH", &new_path) };

    let mut record: Vec<CargoYankTarget> = Vec::new();
    let log = StageLogger::new("test", anodizer_core::log::Verbosity::Normal);
    let result = publish_to_cargo_with(
        &mut ctx,
        &["my-crate".to_string()],
        &log,
        &mut record,
        always_published_1_0_0,
        None,
    );

    // SAFETY: restore PATH.
    unsafe {
        match prev_path {
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
            None => std::env::remove_var("PATH"),
        }
    }

    result.expect("publish must succeed");
    let invocations = read_argv_log(&argv_log);
    let published: Vec<&String> = invocations
        .iter()
        .filter(|l| l.starts_with("publish"))
        .collect();
    assert_eq!(
        published.len(),
        1,
        "cargo publish must be invoked despite unresolvable manifest version: {invocations:?}"
    );
}

// ----- content-vs-version poison guard --------------------------------
//
// These drive `publish_to_cargo_with_guard`, injecting BOTH the crates.io
// already-published index check AND the local `.crate` checksum computer,
// so the guard's match/mismatch/fail-closed branches run without any
// network round-trip or real `cargo package`.

/// `cargo publish -p <name>` count recorded by the stub.
fn publish_count(argv_log: &Path, name: &str) -> usize {
    read_argv_log(argv_log)
        .iter()
        .filter(|l| l.starts_with("publish") && l.contains(name))
        .count()
}

/// version-not-published → guard inert, crate publishes normally.
#[test]
#[serial(cargo_stub_path)]
fn guard_publishes_when_version_not_on_crates_io() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "alpha", "1.0.0");
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("alpha", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["alpha".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let index_absent =
        |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| Ok(None);
    // Local cksum must NEVER be consulted when the version is absent.
    let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        panic!("local cksum must not be computed when version is not published")
    };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["alpha".to_string()],
            &log,
            &mut record,
            index_absent,
            local_panics,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    result.expect("absent version must publish");
    assert_eq!(publish_count(&argv_log, "alpha"), 1, "alpha must publish");
}

/// Fail-CLOSED on an indeterminate working tree: when `project_root` is not
/// a git repository (git status cannot prove cleanliness), the guard must
/// REFUSE — not treat the empty/errored porcelain as "clean → proceed". A
/// guard documented as failing loud rather than silently skipping is a
/// poison hole if an unverifiable tree slips through. Mirrors the real risk:
/// a manual `--publish-only` invoked from a non-repo cwd.
#[test]
#[serial(cargo_stub_path)]
fn guard_refuses_when_git_status_indeterminate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "alpha", "1.0.0");
    // Deliberately NOT a git repo — `git status` errors here.
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("alpha", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["alpha".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // The version is absent so a fail-OPEN guard would proceed to publish;
    // a correct fail-CLOSED guard aborts before ever probing the index.
    let index_absent =
        |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| Ok(None);
    let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        panic!("guard must abort on an unverifiable tree, never package")
    };

    // NB: no `init_clean_repo` here — this fixture's whole point is a
    // non-git `project_root`, so the gate must fail closed.
    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["alpha".to_string()],
            &log,
            &mut record,
            index_absent,
            local_panics,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    let err = result
        .expect_err("an indeterminate (non-git) working tree must fail the guard, not proceed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("cannot verify") && msg.contains("clean git checkout"),
        "error must be actionable about the unverifiable tree: {msg}"
    );
    assert_eq!(
        publish_count(&argv_log, "alpha"),
        0,
        "nothing may publish once the guard refuses: {:?}",
        read_argv_log(&argv_log)
    );
}

/// already-published + local checksum IDENTICAL → safe idempotent skip.
#[test]
#[serial(cargo_stub_path)]
fn guard_skips_when_already_published_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "beta", "2.1.0");
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("beta", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v2.1.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["beta".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let index_match = |_n: &str,
                       _v: &str,
                       _p: &anodizer_core::retry::RetryPolicy,
                       _l: &StageLogger| Ok(Some("abc123".into()));
    let local_match = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "ABC123".to_string(), // case-insensitive match
            bytes: Vec::new(),
        }))
    };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["beta".to_string()],
            &log,
            &mut record,
            index_match,
            local_match,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    result.expect("identical content must be a safe skip");
    assert_eq!(
        publish_count(&argv_log, "beta"),
        0,
        "identical already-published version must NOT re-publish"
    );
}

/// already-published + local content GENUINELY DIFFERENT (not just the
/// vcs commit stamp) → the slow path fetches the published `.crate` and
/// hard-fails on the real drift.
#[test]
#[serial(cargo_stub_path)]
fn guard_hard_fails_when_already_published_different() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "gamma", "3.0.0");
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("gamma", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v3.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["gamma".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let local_bytes = make_crate_tarball(&[
        ("gamma-3.0.0/src/lib.rs", b"fn a() {}"),
        (
            "gamma-3.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a"),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("gamma-3.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
        (
            "gamma-3.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a"),
        ),
    ]);
    let index_sha = sha256_hex(&published_bytes);
    assert_ne!(
        sha256_hex(&local_bytes),
        index_sha,
        "fixture must miss the fast path"
    );

    let index_sha_for_closure = index_sha.clone();
    let index_cksum =
        move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            Ok(Some(index_sha_for_closure.clone()))
        };
    let local_bytes_for_closure = local_bytes.clone();
    let local_differs = move |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: sha256_hex(&local_bytes_for_closure),
            bytes: local_bytes_for_closure.clone(),
        }))
    };
    let published_bytes_for_closure = published_bytes.clone();
    let fetch = move |_n: &str,
                      _v: &str,
                      _p: &anodizer_core::retry::RetryPolicy,
                      _l: &StageLogger| { Ok(published_bytes_for_closure.clone()) };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["gamma".to_string()],
            &log,
            &mut record,
            index_cksum,
            local_differs,
            &fixed_tag_resolver,
            fetch,
            None,
        )
    });
    let err = result.expect_err("content drift must hard-fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("DIFFERENT content")
            && msg.contains("Bump the version")
            && msg.contains("gamma-3.0.0/src/lib.rs"),
        "error must explain the poison, name the differing path, and prescribe a bump: {msg}"
    );
    assert_eq!(
        publish_count(&argv_log, "gamma"),
        0,
        "poisoned version must NOT publish"
    );
}

/// already-published but the crates.io index is UNREACHABLE → fail closed
/// (never silently skip a possibly-poisoned version).
#[test]
#[serial(cargo_stub_path)]
fn guard_fails_closed_when_index_unreachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "delta", "4.2.0");
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("delta", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v4.2.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["delta".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // The dep-completeness probe at the top of the loop also consults this
    // seam; an Err there is treated as Unknown (never fails the guard), so
    // an unreachable index for a no-deps crate is benign until the skip
    // decision, where it must fail closed.
    let index_unreachable =
        |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            Err(anyhow::anyhow!("connection refused"))
        };
    let local_unused = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "unused".to_string(),
            bytes: Vec::new(),
        }))
    };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["delta".to_string()],
            &log,
            &mut record,
            index_unreachable,
            local_unused,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    let err = result.expect_err("unreachable index must fail closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not reach the crates.io index") && msg.contains("possibly-poisoned"),
        "fail-closed error must name the network cause: {msg}"
    );
    assert_eq!(
        publish_count(&argv_log, "delta"),
        0,
        "must NOT publish when the skip decision is unverifiable"
    );
}

/// already-published but the local `.crate` checksum is UNCOMPUTABLE
/// (packaging error) → fail closed; cannot prove identity, refuse to skip.
#[test]
#[serial(cargo_stub_path)]
fn guard_fails_closed_when_local_cksum_uncomputable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "epsilon", "5.0.0");
    let argv_log = tmp.path().join("argv.log");
    let crate_cfg = cargo_crate("epsilon", &path, &[], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .tag("v5.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["epsilon".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let index_present = |_n: &str,
                         _v: &str,
                         _p: &anodizer_core::retry::RetryPolicy,
                         _l: &StageLogger| { Ok(Some("published".into())) };
    let local_errs = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        Err(anyhow::anyhow!("cargo package exploded"))
    };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["epsilon".to_string()],
            &log,
            &mut record,
            index_present,
            local_errs,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    let err = result.expect_err("uncomputable local cksum must fail closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not be computed") && msg.contains("cargo package exploded"),
        "fail-closed error must chain the packaging cause: {msg}"
    );
    assert_eq!(publish_count(&argv_log, "epsilon"), 0, "must not publish");
}

/// Custom (non-crates.io) registry → the crates.io index cksum is
/// meaningless, so the guard is skipped and publish is attempted (the
/// target registry's server governs idempotency). The local-cksum seam
/// must never be consulted.
#[test]
#[serial(cargo_stub_path)]
fn guard_skipped_for_custom_registry_publishes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_crate_dir(tmp.path(), "zeta", "6.0.0");
    let argv_log = tmp.path().join("argv.log");
    let cfg = CargoPublishConfig {
        registry: Some("my-corp".to_string()),
        index_timeout: Some(0),
        ..Default::default()
    };
    let crate_cfg = cargo_crate("zeta", &path, &[], cfg);
    let mut ctx = TestContextBuilder::new()
        .tag("v6.0.0")
        .crates(vec![crate_cfg])
        .selected_crates(vec!["zeta".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // Even if crates.io reports the name+version as published, a custom
    // registry must NOT trust that: attempt publish anyway.
    let index_says_published =
        |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            Ok(Some("crates_io".into()))
        };
    let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        panic!("local cksum must not run for a non-crates.io registry")
    };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["zeta".to_string()],
            &log,
            &mut record,
            index_says_published,
            local_panics,
            &fixed_tag_resolver,
            fetch_panics,
            None,
        )
    });
    result.expect("custom registry publish must proceed");
    assert_eq!(
        publish_count(&argv_log, "zeta"),
        1,
        "custom-registry crate must publish despite a crates.io hit"
    );
}

/// Per-crate workspace mode: EACH published crate is checked independently
/// against its own crates.io entry. crate-a is already published with
/// identical content (skip); crate-b is already published with DIFFERENT
/// content (hard fail) — so the run aborts on b. crate-a (skipped, not
/// published this run) must NOT be recorded for rollback.
#[test]
#[serial(cargo_stub_path)]
fn guard_per_crate_workspace_each_checked_independently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = write_crate_dir(tmp.path(), "ws-a", "0.3.0");
    let path_b = write_crate_dir(tmp.path(), "ws-b", "0.7.0");
    let argv_log = tmp.path().join("argv.log");
    // b depends on a → topological order processes a first.
    let crate_a = cargo_crate("ws-a", &path_a, &[], CargoPublishConfig::default());
    let crate_b = cargo_crate("ws-b", &path_b, &["ws-a"], CargoPublishConfig::default());
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_a, crate_b])
        .selected_crates(vec!["ws-b".to_string()])
        .project_root(tmp.path().to_path_buf())
        .build();
    let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // ws-a: byte-identical re-cut (fast path, no fetch). ws-b: local sha
    // misses the index (slow path), and the fetched published .crate has
    // a genuine content difference (poison → hard fail).
    let ws_b_local_bytes = make_crate_tarball(&[
        ("ws-b-0.7.0/src/lib.rs", b"fn b() {}"),
        (
            "ws-b-0.7.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a"),
        ),
    ]);
    let ws_b_published_bytes = make_crate_tarball(&[
        ("ws-b-0.7.0/src/lib.rs", b"fn b() { /* poisoned */ }"),
        (
            "ws-b-0.7.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a"),
        ),
    ]);
    let ws_b_index_sha = sha256_hex(&ws_b_published_bytes);
    assert_ne!(
        sha256_hex(&ws_b_local_bytes),
        ws_b_index_sha,
        "fixture must miss the fast path for ws-b"
    );

    // Both already published; index cksums differ per crate.
    let ws_b_index_sha_for_closure = ws_b_index_sha.clone();
    let index_per_crate = move |n: &str,
                                _v: &str,
                                _p: &anodizer_core::retry::RetryPolicy,
                                _l: &StageLogger| match n {
        "ws-a" => Ok(Some("a_published".into())),
        "ws-b" => Ok(Some(ws_b_index_sha_for_closure.clone())),
        _ => Ok(None),
    };
    // a matches (safe skip, fast path); b misses the fast path and drifts
    // for real on the slow path (poison → hard fail).
    let ws_b_local_bytes_for_closure = ws_b_local_bytes.clone();
    let local_per_crate =
        move |n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| match n {
            "ws-a" => Ok(Some(LocalCrate {
                cksum: "a_published".to_string(),
                bytes: Vec::new(),
            })),
            "ws-b" => Ok(Some(LocalCrate {
                cksum: sha256_hex(&ws_b_local_bytes_for_closure),
                bytes: ws_b_local_bytes_for_closure.clone(),
            })),
            _ => Ok(None),
        };
    let ws_b_published_bytes_for_closure = ws_b_published_bytes.clone();
    let fetch =
        move |n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            assert_eq!(n, "ws-b", "only ws-b's fast path should miss");
            Ok(ws_b_published_bytes_for_closure.clone())
        };

    let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
    init_clean_repo(tmp.path());
    let result = with_path(&new_path, || {
        publish_to_cargo_with_guard(
            &mut ctx,
            &["ws-b".to_string()],
            &log,
            &mut record,
            index_per_crate,
            local_per_crate,
            &fixed_tag_resolver,
            fetch,
            None,
        )
    });
    let err = result.expect_err("ws-b drift must abort the run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ws-b") && msg.contains("DIFFERENT content"),
        "{msg}"
    );
    // Neither crate published this run; ws-a was a safe skip, ws-b poisoned.
    assert_eq!(
        publish_count(&argv_log, "ws-a"),
        0,
        "ws-a skipped, not published"
    );
    assert_eq!(
        publish_count(&argv_log, "ws-b"),
        0,
        "ws-b poisoned, not published"
    );
    assert!(
        record.is_empty(),
        "no crate published → nothing to roll back"
    );
}
