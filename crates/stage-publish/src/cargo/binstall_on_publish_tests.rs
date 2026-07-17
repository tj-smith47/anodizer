//! binstall-metadata-on-publish tests.

// ---------------------------------------------------------------------------
// binstall-metadata-on-publish tests
//
// The cargo publisher emits [package.metadata.binstall] into each published
// crate's on-disk Cargo.toml right before `cargo publish`, so `cargo binstall`
// fetches the prebuilt asset rather than compiling from source — even on the
// `--publish-only` path that skips the build stage entirely. These tests drive
// `ensure_binstall_metadata_with` with a fixed-tag closure (no git fixture
// needed) across single-crate and workspace per-crate modes, proving the
// emitted overrides carry each crate's OWN name_template / tag / version.
// ---------------------------------------------------------------------------

use super::*;
use anodizer_core::config::{
    ArchiveConfig, ArchivesConfig, BinstallConfig, Defaults, FormatOverride, GitHubConfig,
    ReleaseConfig,
};
use anodizer_core::log::Verbosity;
use anodizer_core::test_helpers::TestContextBuilder;

fn quiet_log() -> StageLogger {
    StageLogger::new("publish-test", Verbosity::Normal)
}

/// `git init` + commit everything under `dir`, yielding a CLEAN working
/// tree the cleanliness gate can verify. The guard fails CLOSED when
/// `git status` cannot prove cleanliness, so a fixture must back its
/// `project_root` with a real repo to exercise the genuine clean-pass.
fn init_clean_repo(dir: &std::path::Path) {
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
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture"]);
}

/// An anodize-style archive: explicit name_template, tar.gz default, windows→zip.
fn anodize_archive() -> ArchiveConfig {
    ArchiveConfig {
        name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}".to_string()),
        formats: Some(vec!["tar.gz".to_string()]),
        format_overrides: Some(vec![FormatOverride {
            os: "windows".to_string(),
            formats: Some(vec!["zip".to_string()]),
        }]),
        ..Default::default()
    }
}

/// A binstall-enabled crate rooted at `path`, owning `name`, with a GitHub
/// release at `tj-smith47/<repo>` and the anodize-style archive.
fn binstall_crate(name: &str, repo: &str, path: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        archives: ArchivesConfig::Configs(vec![anodize_archive()]),
        release: Some(ReleaseConfig {
            github: Some(GitHubConfig {
                owner: "tj-smith47".to_string(),
                name: repo.to_string(),
                token: None,
            }),
            ..Default::default()
        }),
        binstall: Some(BinstallConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn write_manifest(dir: &std::path::Path, name: &str, version: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("Cargo.toml");
    std::fs::write(
        &p,
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
    )
    .unwrap();
    // A binstallable crate is a binary crate; declare the `--bin` so the
    // build-synthesis gate the override derivation routes through sees a
    // producing default build (a no-bin crate now derives no targets).
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    p
}

/// Read the emitted override asset leaf for `triple`, resolving the
/// cargo-binstall `{ version }` token back to `version`.
fn override_asset(manifest: &std::path::Path, triple: &str, version: &str) -> String {
    let doc = std::fs::read_to_string(manifest)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let url = doc["package"]["metadata"]["binstall"]["overrides"][triple]["pkg-url"]
        .as_str()
        .unwrap()
        .to_string();
    url.rsplit('/')
        .next()
        .unwrap()
        .replace("{ version }", version)
}

/// Single-crate mode: a lone crate gets its binstall overrides emitted with
/// its own name_template, resolving to the real per-target asset names.
#[test]
fn single_crate_emits_binstall_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("app");
    let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");

    let crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .crates(vec![crate_cfg.clone()])
        .build();

    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
    ensure_binstall_metadata_with(&mut ctx, &crate_cfg, false, &quiet_log(), &fixed_tag).unwrap();

    assert_eq!(
        override_asset(&manifest, "x86_64-unknown-linux-gnu", "1.2.3"),
        "anodizer-1.2.3-linux-amd64.tar.gz"
    );
    assert_eq!(
        override_asset(&manifest, "aarch64-pc-windows-msvc", "1.2.3"),
        "anodizer-1.2.3-windows-arm64.zip"
    );
}

/// Disabled binstall is a no-op: the manifest is left pristine.
#[test]
fn disabled_binstall_does_not_mutate_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("app");
    let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");
    let original = std::fs::read_to_string(&manifest).unwrap();

    let mut crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
    crate_cfg.binstall = Some(BinstallConfig {
        enabled: Some(false),
        ..Default::default()
    });
    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .crates(vec![crate_cfg.clone()])
        .build();

    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
    ensure_binstall_metadata_with(&mut ctx, &crate_cfg, false, &quiet_log(), &fixed_tag).unwrap();
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        original,
        "disabled binstall must leave the manifest untouched"
    );
}

/// dry_run honored: the emitter does not mutate the manifest under dry-run.
#[test]
fn dry_run_does_not_mutate_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("app");
    let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");
    let original = std::fs::read_to_string(&manifest).unwrap();

    let crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .crates(vec![crate_cfg.clone()])
        .build();

    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
    ensure_binstall_metadata_with(&mut ctx, &crate_cfg, true, &quiet_log(), &fixed_tag).unwrap();
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        original,
        "dry-run binstall emission must leave the manifest untouched"
    );
}

/// Workspace per-crate mode: two crates with DIFFERENT versions, repos, and
/// (via the fixed-tag closure) tags. Each crate's emitted overrides must
/// carry its OWN version/repo — never a shared/global value — proving the
/// per-crate re-scope. This is the canonical anodize-only bug family the
/// all-config-modes rule guards against.
#[test]
fn workspace_per_crate_emits_each_crates_own_version_and_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_a = tmp.path().join("crate-a");
    let dir_b = tmp.path().join("crate-b");
    let manifest_a = write_manifest(&dir_a, "alpha", "1.0.0");
    let manifest_b = write_manifest(&dir_b, "beta", "2.5.0");

    let crate_a = binstall_crate("alpha", "alpha-repo", dir_a.to_str().unwrap());
    let crate_b = binstall_crate("beta", "beta-repo", dir_b.to_str().unwrap());

    let mut ctx = TestContextBuilder::new()
        .project_name("alpha")
        .crates(vec![crate_a.clone(), crate_b.clone()])
        .build();

    // Each crate resolves to its OWN tag — a per-crate-cadence workspace.
    let tag_a = |_: &Context, _: &CrateConfig| Some("v1.0.0".to_string());
    let tag_b = |_: &Context, _: &CrateConfig| Some("v2.5.0".to_string());

    ensure_binstall_metadata_with(&mut ctx, &crate_a, false, &quiet_log(), &tag_a).unwrap();
    ensure_binstall_metadata_with(&mut ctx, &crate_b, false, &quiet_log(), &tag_b).unwrap();

    // crate-a: alpha @ 1.0.0 at alpha-repo.
    assert_eq!(
        override_asset(&manifest_a, "x86_64-unknown-linux-gnu", "1.0.0"),
        "alpha-1.0.0-linux-amd64.tar.gz"
    );
    let doc_a = std::fs::read_to_string(&manifest_a)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let url_a = doc_a["package"]["metadata"]["binstall"]["overrides"]
            ["x86_64-unknown-linux-gnu"]["pkg-url"]
            .as_str()
            .unwrap();
    assert!(
        url_a.contains("tj-smith47/alpha-repo") && url_a.contains("/v{ version }/"),
        "crate-a override must target its OWN repo + tag token, got: {url_a}"
    );

    // crate-b: beta @ 2.5.0 at beta-repo — NOT alpha's version/repo.
    assert_eq!(
        override_asset(&manifest_b, "aarch64-apple-darwin", "2.5.0"),
        "beta-2.5.0-darwin-arm64.tar.gz"
    );
    let doc_b = std::fs::read_to_string(&manifest_b)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let url_b =
        doc_b["package"]["metadata"]["binstall"]["overrides"]["aarch64-apple-darwin"]["pkg-url"]
            .as_str()
            .unwrap();
    assert!(
        url_b.contains("tj-smith47/beta-repo"),
        "crate-b override must target its OWN repo, not crate-a's, got: {url_b}"
    );
    assert!(
        !url_b.contains("alpha"),
        "crate-b override must not leak crate-a's name/version, got: {url_b}"
    );
}

/// `defaults.targets` drives the override set when no per-build targets are
/// configured — `resolve_default_targets` must mirror the build stage so the
/// emitted triples equal the released asset set.
#[test]
fn resolve_default_targets_honors_config_then_falls_back() {
    // Explicit defaults.targets wins.
    let ctx = TestContextBuilder::new()
        .defaults(Defaults {
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        })
        .build();
    assert_eq!(
        resolve_default_targets(&ctx),
        vec!["x86_64-unknown-linux-gnu".to_string()]
    );

    // No defaults.targets → canonical DEFAULT_TARGETS (the six-triple matrix).
    let ctx2 = TestContextBuilder::new().build();
    assert_eq!(
        resolve_default_targets(&ctx2),
        anodizer_core::target::DEFAULT_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
}

// -----------------------------------------------------------------------
// Guard-ordering tests: the poison guard must package the SAME tree state
// `cargo publish` uploads, including anodizer's own pre-publish binstall
// mutation. These drive the full `publish_to_cargo_with_guard` loop with an
// injected local-cksum that READS the on-disk Cargo.toml, so the recorded
// hash reflects whether the binstall table was written before the guard ran.
// -----------------------------------------------------------------------

/// True when the crate at `path` carries `[package.metadata.binstall]` in
/// its on-disk Cargo.toml. The stand-in for "the .crate bytes differ with
/// vs without the binstall table" — without re-implementing `cargo package`.
fn has_binstall_table(path: &str) -> bool {
    let manifest = std::path::Path::new(path).join("Cargo.toml");
    std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .map(|doc| {
            doc.get("package")
                .and_then(|p| p.get("metadata"))
                .and_then(|m| m.get("binstall"))
                .is_some()
        })
        .unwrap_or(false)
}

/// A cargo cfg with the post-publish index poll disabled (no dependents in
/// these single-crate fixtures) so the loop never waits on the real index.
fn no_poll_cargo_cfg() -> CargoPublishConfig {
    CargoPublishConfig {
        index_timeout: Some(0),
        ..Default::default()
    }
}

fn binstall_crate_for_publish(name: &str, repo: &str, path: &str) -> CrateConfig {
    let mut c = binstall_crate(name, repo, path);
    c.publish = Some(anodizer_core::config::PublishConfig {
        cargo: Some(no_poll_cargo_cfg()),
        ..Default::default()
    });
    c
}

/// Fetch closure that panics if invoked — for guard tests whose local
/// cksum matches the index (fast path) or never reaches the download.
fn fetch_panics(
    _: &str,
    _: &str,
    _: &anodizer_core::retry::RetryPolicy,
    _: &StageLogger,
) -> Result<Vec<u8>> {
    panic!("fetch_published must not run on this path")
}

/// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
/// given `(in-tar path, content)` entries — for the negative-control test
/// that must exercise the slow-path content comparison with real bytes.
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

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
}

/// BLOCKER reproduction: a binstall crate, already published with the
/// WITH-binstall content (as the original publish uploaded), must be a SAFE
/// SKIP on re-cut — NOT a false poison. The guard now writes the binstall
/// table before packaging, so the local hash reflects the same tree the
/// original `cargo publish` shipped. (Before the fix, the guard packaged the
/// pre-binstall tree → local "WITHOUT" ≠ index "WITH" → false hard-fail.)
#[test]
fn guard_skips_binstall_crate_when_recut_matches_published() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("cli");
    write_manifest(&crate_dir, "anodizer", "1.2.3");
    let path = crate_dir.to_str().unwrap();
    let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);

    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .tag("v1.2.3")
        .crates(vec![crate_cfg.clone()])
        .selected_crates(vec!["anodizer".to_string()])
        .build();
    // Commit the fixture tree so the cleanliness gate verifies a genuine
    // CLEAN repo (not the old fail-open hole), isolating the skip path.
    init_clean_repo(tmp.path());
    ctx.options.project_root = Some(tmp.path().to_path_buf());
    let log = quiet_log();
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // The version is on crates.io; its recorded cksum is the WITH-binstall
    // marker (what the original publish, which wrote the table, uploaded).
    let index_with_binstall =
        |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            Ok(Some("WITH".into()))
        };
    // The local-cksum stub hashes the REAL on-disk tree: "WITH" iff the
    // binstall table is present at the moment the guard packages.
    let local_reads_disk = |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        let marker = if has_binstall_table(&c.path) {
            "WITH"
        } else {
            "WITHOUT"
        };
        Ok(Some(LocalCrate {
            cksum: marker.to_string(),
            bytes: Vec::new(),
        }))
    };
    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());

    publish_to_cargo_with_guard(
        &mut ctx,
        &["anodizer".to_string()],
        &log,
        &mut record,
        index_with_binstall,
        local_reads_disk,
        &fixed_tag,
        fetch_panics,
        None,
    )
    .expect(
        "a binstall crate re-cut whose published content already includes the binstall table \
             must be a SAFE SKIP, not a false poison",
    );
    assert!(
        record.is_empty(),
        "a safe skip publishes nothing — nothing to record"
    );
}

/// Negative control proving the fix is load-bearing: if the index recorded
/// the WITHOUT-binstall content (a crate published BEFORE anodizer started
/// writing the table), the guard — which now packages WITH the table — would
/// see a genuine content divergence and hard-fail. This demonstrates the
/// guard still flags real drift; it isn't blanket-skipping binstall crates.
#[test]
fn guard_flags_real_drift_even_for_binstall_crate() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("cli");
    write_manifest(&crate_dir, "anodizer", "9.9.9");
    let path = crate_dir.to_str().unwrap();
    let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);

    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .tag("v9.9.9")
        .crates(vec![crate_cfg.clone()])
        .selected_crates(vec!["anodizer".to_string()])
        .build();
    // Committed clean repo (see the sibling skip test) → the gate passes
    // on merit and this test exercises the genuine drift hard-fail.
    init_clean_repo(tmp.path());
    ctx.options.project_root = Some(tmp.path().to_path_buf());
    let log = quiet_log();
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // Real tarball bytes standing in for "packaged WITH the binstall
    // table" vs "published WITHOUT it" — a genuine content divergence,
    // not just the vcs commit stamp, so the slow path must hard-fail.
    let with_binstall_bytes = make_crate_tarball(&[(
        "anodizer-9.9.9/Cargo.toml",
        b"[package]\nname = \"anodizer\"\n\n[package.metadata.binstall]\npkg-url = \"x\"\n",
    )]);
    let without_binstall_bytes = make_crate_tarball(&[(
        "anodizer-9.9.9/Cargo.toml",
        b"[package]\nname = \"anodizer\"\n",
    )]);
    let index_sha = sha256_hex(&without_binstall_bytes);

    let index_sha_for_closure = index_sha.clone();
    let index_without_binstall =
        move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
            Ok(Some(index_sha_for_closure.clone()))
        };
    let with_binstall_bytes_for_local = with_binstall_bytes.clone();
    let without_binstall_bytes_for_local = without_binstall_bytes.clone();
    let local_reads_disk = move |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        // The guard's pre-publish mutation writes the binstall table
        // before packaging; a real `cargo package` here would reflect it,
        // so the stub packages the "WITH" fixture whenever the on-disk
        // manifest carries the table (as it does after that mutation).
        let bytes = if has_binstall_table(&c.path) {
            with_binstall_bytes_for_local.clone()
        } else {
            without_binstall_bytes_for_local.clone()
        };
        Ok(Some(LocalCrate {
            cksum: sha256_hex(&bytes),
            bytes,
        }))
    };
    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v9.9.9".to_string());
    let fetch = move |_n: &str,
                      _v: &str,
                      _p: &anodizer_core::retry::RetryPolicy,
                      _l: &StageLogger| { Ok(without_binstall_bytes.clone()) };

    let err = publish_to_cargo_with_guard(
        &mut ctx,
        &["anodizer".to_string()],
        &log,
        &mut record,
        index_without_binstall,
        local_reads_disk,
        &fixed_tag,
        fetch,
        None,
    )
    .expect_err("a genuine content divergence must still hard-fail");
    assert!(
        format!("{err:#}").contains("DIFFERENT content"),
        "must report the poison, not silently skip: {err:#}"
    );
}

/// Multi-crate regression: crate A's pre-publish binstall write dirties the
/// tree, but it must NOT false-trip the cleanliness check for crate B. The
/// check runs ONCE before the loop, on a tree clean at entry, so both
/// binstall crates re-cut safely. (A per-crate check would have seen A's
/// write and wrongly aborted B.)
#[test]
fn guard_clean_check_runs_once_not_per_crate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    write_manifest(&dir_a, "alpha", "1.0.0");
    write_manifest(&dir_b, "beta", "1.0.0");
    let mut crate_a = binstall_crate_for_publish("alpha", "alpha", dir_a.to_str().unwrap());
    // b depends on a → topological order processes a first, so a's binstall
    // write lands before b's iteration.
    crate_a.depends_on = Some(vec![]);
    let mut crate_b = binstall_crate_for_publish("beta", "beta", dir_b.to_str().unwrap());
    crate_b.depends_on = Some(vec!["alpha".to_string()]);

    let mut ctx = TestContextBuilder::new()
        .project_name("alpha")
        .tag("v1.0.0")
        .crates(vec![crate_a, crate_b])
        .selected_crates(vec!["beta".to_string()])
        .build();
    // Committed clean repo → the once-before-loop gate passes on merit;
    // crate A's later in-loop binstall write must NOT retroactively trip it.
    init_clean_repo(tmp.path());
    ctx.options.project_root = Some(tmp.path().to_path_buf());
    let log = quiet_log();
    let mut record: Vec<CargoYankTarget> = Vec::new();

    // Both already published with WITH-binstall content → both safe skip.
    let index_with = |_n: &str,
                      _v: &str,
                      _p: &anodizer_core::retry::RetryPolicy,
                      _l: &StageLogger| Ok(Some("WITH".into()));
    let local_reads_disk = |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        let m = if has_binstall_table(&c.path) {
            "WITH"
        } else {
            "WITHOUT"
        };
        Ok(Some(LocalCrate {
            cksum: m.to_string(),
            bytes: Vec::new(),
        }))
    };
    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.0.0".to_string());

    publish_to_cargo_with_guard(
        &mut ctx,
        &["beta".to_string()],
        &log,
        &mut record,
        index_with,
        local_reads_disk,
        &fixed_tag,
        fetch_panics,
        None,
    )
    .expect("crate A's binstall write must not false-trip the dirty check for crate B");
    assert!(
        record.is_empty(),
        "both crates safe-skipped → nothing recorded"
    );
}

/// WARN coverage: a DIRTY working tree at guard entry is an unverifiable
/// precondition — the guard must STOP with an actionable error (not skip,
/// not hard-fail on content). Uses a real git fixture so
/// `git status --porcelain` reports the uncommitted change.
#[test]
fn guard_refuses_dirty_tree_before_binstall_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    // A minimal git repo with one committed crate, then an uncommitted edit.
    let run_git = |args: &[&str]| {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(repo).args(args);
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {args:?} failed");
    };
    run_git(&["init", "-q"]);
    run_git(&["config", "user.email", "t@example.com"]);
    run_git(&["config", "user.name", "t"]);
    let crate_dir = repo.join("cli");
    write_manifest(&crate_dir, "anodizer", "1.2.3");
    run_git(&["add", "-A"]);
    run_git(&["commit", "-qm", "init"]);
    // Dirty the tree: an uncommitted source edit.
    std::fs::write(crate_dir.join("extra.rs"), "// uncommitted\n").unwrap();

    let path = crate_dir.to_str().unwrap();
    let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);
    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .tag("v1.2.3")
        .crates(vec![crate_cfg.clone()])
        .selected_crates(vec!["anodizer".to_string()])
        .build();
    // Point the cleanliness check at the fixture repo, not the process cwd.
    ctx.options.project_root = Some(repo.to_path_buf());
    let log = quiet_log();
    let mut record: Vec<CargoYankTarget> = Vec::new();

    let index_present = |_n: &str,
                         _v: &str,
                         _p: &anodizer_core::retry::RetryPolicy,
                         _l: &StageLogger| Ok(Some("WITH".into()));
    // Must never be reached — the dirty check aborts before packaging.
    let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
        panic!("local cksum must not run against a dirty tree")
    };
    let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());

    let err = publish_to_cargo_with_guard(
        &mut ctx,
        &["anodizer".to_string()],
        &log,
        &mut record,
        index_present,
        local_panics,
        &fixed_tag,
        fetch_panics,
        None,
    )
    .expect_err("a dirty tree is an unverifiable precondition; the guard must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("DIRTY") && msg.contains("clean checkout") && msg.contains("extra.rs"),
        "error must be actionable (name the dirtiness + the remedy): {msg}"
    );
}
