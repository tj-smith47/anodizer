use super::*;
use crate::util::CommitOutcome;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, GitRepoConfig, HomebrewCaskCompletions, HomebrewCaskConfig,
    HomebrewCaskGeneratedCompletions, HomebrewCaskURL, HomebrewConfig, HomebrewDependency,
    HomebrewLivecheck, PublishConfig, PullRequestConfig, ReleaseConfig, RepositoryConfig,
    StringOrBool, WorkspaceConfig,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::fake_tool::{FakeToolDir, PathGuard};
use serial_test::serial;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[test]
fn commit_outcome_is_pushed() {
    assert!(CommitOutcome::Pushed.is_pushed());
    assert!(!CommitOutcome::NoChanges.is_pushed());
}

fn quiet_log() -> StageLogger {
    StageLogger::new("homebrew-test", Verbosity::Quiet)
}

/// Install a `gh` stub that exits non-zero on `--version` so the PR
/// transport's `gh_is_available()` probe reports false, then prepend it
/// to `PATH`. This makes the PR submission path deterministic (it routes
/// to the gh-absent / token-driven fallback instead of a LIVE
/// `gh pr create` against github.com on a host that has a real,
/// authenticated `gh` in PATH). Returns the `FakeToolDir` holder (keeps
/// the stub on disk) plus the `PathGuard` (restores `PATH` + releases the
/// env mutex on drop) — both must be held for the test's duration. Tests
/// using this MUST be `#[serial(path_env)]` because the guard mutates
/// process `PATH`. Mirrors `util/pr.rs::gh_absent_path`.
fn gh_absent() -> (FakeToolDir, PathGuard) {
    let tools = FakeToolDir::new();
    tools.tool("gh").exit(1).install();
    let guard = tools.activate();
    (tools, guard)
}

fn git_ok(dir: &Path, args: &[&str]) {
    anodizer_core::test_helpers::git_test_ok(dir, args)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    anodizer_core::test_helpers::git_test_stdout(dir, args)
}

/// Build a bare tap repo seeded with one commit on `branch`. Returns the
/// bare repo path (a usable local `git clone` URL) plus the holder
/// tempdir. The publisher clones this via the `git.url` SSH branch
/// (which is a plain `git clone <localpath>` for a filesystem path),
/// commits the formula, and pushes back to it. The seeded bare repo is
/// the assertion surface: we inspect its landed `.rb` content + the
/// commit subject after the publish.
fn make_bare_tap(branch: &str) -> (String, tempfile::TempDir) {
    let bare = tempfile::tempdir().expect("bare tempdir");
    let seed = tempfile::tempdir().expect("seed tempdir");

    git_ok(bare.path(), &["init", "--bare", "-b", branch]);
    git_ok(seed.path(), &["init", "-b", branch]);
    git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
    git_ok(seed.path(), &["config", "user.name", "T"]);
    git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.path().join("README"), "tap\n").unwrap();
    git_ok(seed.path(), &["add", "README"]);
    git_ok(seed.path(), &["commit", "-m", "seed tap"]);
    // `git remote add` takes a path; pass it as an OsStr arg.
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["remote", "add", "origin"])
                    .arg(bare.path())
                    .current_dir(seed.path());
                cmd
            },
            "git",
        )
        .status
        .success(),
        "git remote add origin failed"
    );
    git_ok(seed.path(), &["push", "-u", "origin", branch]);
    (bare.path().to_string_lossy().into_owned(), bare)
}

/// Read the rendered formula `.rb` that landed on the bare tap's
/// `branch` ref (formula lives at the tap root unless `directory:` is
/// set). Uses `git show <branch>:<path>` so we read the pushed object,
/// not a stale working tree.
fn tap_show(bare: &Path, branch: &str, path: &str) -> String {
    git_stdout(bare, &["show", &format!("{branch}:{path}")])
}

/// Archive artifact carrying url + sha256 + format metadata for `mytool`.
fn archive(target: &str, url: &str, sha: &str) -> Artifact {
    let mut metadata = HashMap::new();
    metadata.insert("url".to_string(), url.to_string());
    metadata.insert("sha256".to_string(), sha.to_string());
    metadata.insert("format".to_string(), "tar.gz".to_string());
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/tmp/{target}.tar.gz")),
        name: format!("mytool-{target}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    }
}

/// A `HomebrewConfig` whose `git.url` points the clone at a local bare
/// tap, with `owner`/`name`/`branch` set so owner-name resolution and the
/// push target match the seeded ref.
fn hb_cfg_local(bare_url: &str, branch: &str) -> HomebrewConfig {
    HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            branch: Some(branch.to_string()),
            git: Some(GitRepoConfig {
                url: Some(bare_url.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a single-crate (top-level) Context wired to publish `mytool` to
/// the homebrew tap with the supplied artifacts. Version resolves to
/// `1.2.3` (tag `v1.2.3` via the builder default).
fn single_crate_ctx(hb: HomebrewConfig, artifacts: Vec<Artifact>) -> Context {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            release: Some(ReleaseConfig {
                github: Some(anodizer_core::config::ScmRepoConfig {
                    owner: "myorg".to_string(),
                    name: "mytool".to_string(),
                    token: None,
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                homebrew: Some(hb),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    for a in artifacts {
        ctx.artifacts.add(a);
    }
    ctx
}

// ===================================================================
// collect_archive_entries / homebrew_matching_artifacts — filter +
// disambiguation + error paths feeding the formula renderer.
// ===================================================================

/// `collect_archive_entries` returns one `(target,url,sha256)` tuple per
/// matching archive, carrying the artifact's url + sha256 verbatim.
#[test]
fn collect_archive_entries_returns_url_sha_per_archive() {
    let hb = HomebrewConfig::default();
    let ctx = single_crate_ctx(
        hb.clone(),
        vec![
            archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
            archive(
                "x86_64-unknown-linux-gnu",
                "https://e/linux.tar.gz",
                "shalin",
            ),
        ],
    );
    let got = collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log()).expect("collect");
    assert_eq!(got.len(), 2);
    let arm = got
        .iter()
        .find(|(t, _, _)| t == "aarch64-apple-darwin")
        .expect("arm entry");
    assert_eq!(arm.1, "https://e/arm.tar.gz");
    assert_eq!(arm.2, "shaarm");
}

/// A matched artifact missing `sha256` is a real defect: the formula
/// would fail `brew audit`. `collect_archive_entries` must `Err` naming
/// the artifact + the checksum-stage remediation, not emit an empty sha.
#[test]
fn collect_archive_entries_errors_on_missing_sha256() {
    let hb = HomebrewConfig::default();
    let mut art = archive("x86_64-unknown-linux-gnu", "https://e/linux.tar.gz", "x");
    art.metadata.remove("sha256");
    let ctx = single_crate_ctx(hb.clone(), vec![art]);
    let err = collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log())
        .expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("sha256"), "{msg}");
    assert!(msg.contains("checksum stage"), "{msg}");
}

/// `url_template` overrides the artifact's url metadata: the rendered URL
/// is computed from the template (os/arch/version) rather than copied
/// from `metadata.url`. Proves the template branch of the url resolver.
#[test]
fn collect_archive_entries_renders_url_template() {
    let hb = HomebrewConfig {
        url_template: Some("https://dl/{{ .Version }}/{{ .Os }}-{{ .Arch }}.tar.gz".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb.clone(),
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://ignored/original.tar.gz",
            "shalin",
        )],
    );
    let got = collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log()).expect("collect");
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].1, "https://dl/1.2.3/linux-amd64.tar.gz",
        "url_template must drive the download URL, not metadata.url"
    );
}

/// The `ids:` allow-list filters the archive set: an artifact whose `id`
/// is not listed is dropped from the formula's candidate set.
#[test]
fn homebrew_matching_artifacts_honors_ids_allowlist() {
    let hb = HomebrewConfig {
        ids: Some(vec!["keepme".to_string()]),
        ..Default::default()
    };
    let mut keep = archive("aarch64-apple-darwin", "https://e/keep.tar.gz", "k");
    keep.metadata.insert("id".to_string(), "keepme".to_string());
    let mut drop = archive("x86_64-unknown-linux-gnu", "https://e/drop.tar.gz", "d");
    drop.metadata.insert("id".to_string(), "other".to_string());
    let ctx = single_crate_ctx(hb.clone(), vec![keep, drop]);
    let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
    assert_eq!(matched.len(), 1, "only the allow-listed id survives");
    assert_eq!(
        matched[0].metadata.get("id").map(|s| s.as_str()),
        Some("keepme")
    );
}

/// A typed `amd64_variant: v3` selector matches the v3-tagged amd64
/// archive and drops the explicitly-v1-tagged one — the positive half of
/// the enum conversion (a typo'd level now dies at config parse; a valid
/// level keeps selecting exactly the tuned archive).
#[test]
fn homebrew_matching_artifacts_selects_declared_amd64_variant() {
    let hb = HomebrewConfig {
        amd64_variant: Some(anodizer_core::config::Amd64Variant::V3),
        ..Default::default()
    };
    let mut v3 = archive("x86_64-unknown-linux-gnu", "https://e/v3.tar.gz", "s3");
    v3.metadata
        .insert("amd64_variant".to_string(), "v3".to_string());
    let mut v1 = archive("x86_64-unknown-linux-gnu", "https://e/v1.tar.gz", "s1");
    v1.metadata
        .insert("amd64_variant".to_string(), "v1".to_string());
    let ctx = single_crate_ctx(hb.clone(), vec![v3, v1]);
    let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
    assert_eq!(
        matched.len(),
        1,
        "only the v3-tagged archive matches a v3 selector"
    );
    assert_eq!(
        matched[0].metadata.get("url").map(String::as_str),
        Some("https://e/v3.tar.gz")
    );
}

/// A raw single-file `gz` blob (not `tar.gz`) cannot be installed as a
/// Homebrew archive; the presence probe excludes it.
#[test]
fn homebrew_matching_artifacts_excludes_raw_gz() {
    let hb = HomebrewConfig::default();
    let mut gz = archive("x86_64-unknown-linux-gnu", "https://e/blob.gz", "g");
    gz.metadata.insert("format".to_string(), "gz".to_string());
    let ctx = single_crate_ctx(hb.clone(), vec![gz]);
    assert!(
        homebrew_matching_artifacts(&ctx, &hb, "mytool").is_empty(),
        "a raw .gz blob must not count as a homebrew archive candidate"
    );
    assert!(
        !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
        "crate_has_homebrew_archives must agree with the presence probe"
    );
}

/// Homebrew installs on macOS + Linux only: a windows archive is NOT an
/// eligible candidate, so the presence probe (and thus
/// `crate_has_homebrew_archives`) excludes it. Guards the failure-hiding
/// class where a windows `.zip` would otherwise render a flat windows-url
/// formula that 404s `brew install` on macOS/Linux.
#[test]
fn homebrew_matching_artifacts_excludes_windows() {
    let hb = HomebrewConfig::default();
    let win = archive("x86_64-pc-windows-msvc", "https://e/win.zip", "w");
    let mac = archive("aarch64-apple-darwin", "https://e/mac.tar.gz", "m");
    let ctx = single_crate_ctx(hb.clone(), vec![win, mac]);
    let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
    assert_eq!(matched.len(), 1, "only the macOS archive is eligible");
    assert_eq!(
        matched[0].target.as_deref(),
        Some("aarch64-apple-darwin"),
        "the windows archive must be filtered out"
    );
}

/// The apple-non-macOS targets (`*-apple-ios`/`-tvos`/`-watchos`) are
/// buildable but carry no `brew`-installable binary; the broad `is_darwin`
/// ("apple") predicate would wrongly admit them (they land in the formula's
/// untyped `# platform:` url block — a 404-class install). The macOS-specific
/// `is_macos` eligibility must exclude them while keeping genuine macOS.
#[test]
fn homebrew_matching_artifacts_excludes_apple_non_macos() {
    let hb = HomebrewConfig::default();
    let ios = archive("aarch64-apple-ios", "https://e/ios.tar.gz", "i");
    let tvos = archive("aarch64-apple-tvos", "https://e/tvos.tar.gz", "t");
    let watchos = archive("aarch64-apple-watchos", "https://e/watchos.tar.gz", "w");
    let mac = archive("aarch64-apple-darwin", "https://e/mac.tar.gz", "m");
    let ctx = single_crate_ctx(hb.clone(), vec![ios, tvos, watchos, mac]);
    let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
    assert_eq!(
        matched.len(),
        1,
        "only the genuine macOS archive is eligible; ios/tvos/watchos excluded"
    );
    assert_eq!(
        matched[0].target.as_deref(),
        Some("aarch64-apple-darwin"),
        "the apple-non-macOS archives must be filtered out"
    );
}

/// A target-less archive (no triple) matches neither `is_macos` nor
/// `is_linux`, so the OS filter excludes it — the presence probe reports
/// absence rather than routing it through a flat-url formula. Documents the
/// intended behavior of the `unwrap_or("")` fallback in the filter.
#[test]
fn homebrew_matching_artifacts_excludes_target_less() {
    let hb = HomebrewConfig::default();
    let mut targetless = archive("x86_64-unknown-linux-gnu", "https://e/x.tar.gz", "s");
    targetless.target = None;
    let ctx = single_crate_ctx(hb.clone(), vec![targetless]);
    assert!(
        homebrew_matching_artifacts(&ctx, &hb, "mytool").is_empty(),
        "a target-less archive is not a homebrew candidate"
    );
    assert!(
        !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
        "crate_has_homebrew_archives must agree the target-less set is absent"
    );
}

/// A windows-ONLY artifact set carries no homebrew-eligible archive, so the
/// presence probe reports absence — mirroring nix's `Ok(false)` for a
/// windows-only shard, which lets the emission validator self-skip.
#[test]
fn crate_has_homebrew_archives_false_for_windows_only() {
    let hb = HomebrewConfig::default();
    let win = archive("x86_64-pc-windows-msvc", "https://e/win.zip", "w");
    let ctx = single_crate_ctx(hb.clone(), vec![win]);
    assert!(
        !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
        "a windows-only set is not homebrew-eligible"
    );
}

/// `crate_has_homebrew_archives` is presence-only: a matched artifact with
/// NO url/sha256 still returns true (the caller surfaces the broken
/// metadata via the render `Err`, not a silent skip).
#[test]
fn crate_has_homebrew_archives_true_even_when_metadata_incomplete() {
    let hb = HomebrewConfig::default();
    let mut art = archive("x86_64-unknown-linux-gnu", "https://e/x.tar.gz", "s");
    art.metadata.remove("url");
    art.metadata.remove("sha256");
    let ctx = single_crate_ctx(hb.clone(), vec![art]);
    assert!(
        crate_has_homebrew_archives(&ctx, &hb, "mytool"),
        "presence probe must report present-but-broken artifacts as present"
    );
}

// ===================================================================
// render_homebrew_formula_for_crate / render_formula_inner — the Ruby
// body the publisher would write.
// ===================================================================

/// The rendered formula carries the PascalCase class name, the version,
/// each archive url + sha256, and a dependency declaration. Pins the
/// load-bearing formula content the tap commit would carry.
#[test]
fn render_formula_for_crate_emits_class_url_sha_and_deps() {
    let hb = HomebrewConfig {
        dependencies: Some(vec![HomebrewDependency {
            name: "openssl".to_string(),
            os: None,
            dep_type: None,
            version: None,
        }]),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![
            archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
            archive(
                "x86_64-unknown-linux-gnu",
                "https://e/linux.tar.gz",
                "shalin",
            ),
        ],
    );
    let rendered = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render ok")
        .expect("not skipped");
    let body = &rendered.formula;
    assert_eq!(rendered.formula_name, "mytool");
    assert!(
        body.contains("class Mytool < Formula"),
        "class line:\n{body}"
    );
    assert!(body.contains("version \"1.2.3\""), "version:\n{body}");
    assert!(body.contains("https://e/arm.tar.gz"), "arm url:\n{body}");
    assert!(body.contains("shaarm"), "arm sha:\n{body}");
    assert!(
        body.contains("https://e/linux.tar.gz"),
        "linux url:\n{body}"
    );
    assert!(body.contains("depends_on \"openssl\""), "dep:\n{body}");
}

/// `name:` override changes both the rendered class token and the
/// `formula_name` (the `.rb` filename stem the publisher writes).
#[test]
fn render_formula_for_crate_honors_name_override() {
    let hb = HomebrewConfig {
        name: Some("rebranded".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let rendered = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped");
    assert_eq!(rendered.formula_name, "rebranded");
    assert!(
        rendered.formula.contains("class Rebranded < Formula"),
        "{}",
        rendered.formula
    );
}

/// `skip_upload: true` makes the render-for-validation entry return
/// `Ok(None)` (nothing to render) — distinct from an error.
#[test]
fn render_formula_for_crate_skip_upload_returns_none() {
    let hb = HomebrewConfig {
        skip_upload: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let got = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log()).expect("ok");
    assert!(got.is_none(), "skip_upload=true must render None");
}

/// A falsy `if:` condition skips the render (returns `Ok(None)`).
#[test]
fn render_formula_for_crate_falsy_if_returns_none() {
    let hb = HomebrewConfig {
        if_condition: Some("false".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let got = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log()).expect("ok");
    assert!(got.is_none(), "falsy `if` must render None");
}

// ===================================================================
// publish_to_homebrew — full clone → write → commit → push round-trip
// against a local bare tap (direct-push path; PR disabled by default).
// ===================================================================

/// Happy path, single-crate mode: the publisher clones the local bare
/// tap, writes `mytool.rb`, commits, and pushes. Asserts (1) the return
/// is `Ok(true)` (a real push happened), (2) the formula `.rb` landed on
/// the tap's branch ref with the correct class + version + url + sha, and
/// (3) the commit subject names the formula + version.
#[test]
fn publish_to_homebrew_direct_push_lands_formula_single_crate() {
    let (bare_url, bare) = make_bare_tap("main");
    let hb = hb_cfg_local(&bare_url, "main");
    let mut ctx = single_crate_ctx(
        hb,
        vec![
            archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
            archive(
                "x86_64-unknown-linux-gnu",
                "https://e/linux.tar.gz",
                "shalin",
            ),
        ],
    );
    let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    assert!(pushed, "a real push must return Ok(true)");

    let bare_path = Path::new(&bare_url);
    let formula = tap_show(bare_path, "main", "mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    assert!(formula.contains("version \"1.2.3\""), "{formula}");
    assert!(formula.contains("https://e/arm.tar.gz"), "{formula}");
    assert!(formula.contains("shalin"), "{formula}");

    let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
    assert!(
        subject.contains("mytool") && subject.contains("1.2.3"),
        "commit subject must name formula + version; got: {subject}"
    );
    drop(bare);
}

/// `directory:` places the formula in a sub-tree of the tap. Asserts the
/// pushed object lives at `Formula/mytool.rb`, not the tap root.
#[test]
fn publish_to_homebrew_writes_into_configured_directory() {
    let (bare_url, bare) = make_bare_tap("main");
    let mut hb = hb_cfg_local(&bare_url, "main");
    hb.directory = Some("Formula".to_string());
    let mut ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");

    let bare_path = Path::new(&bare_url);
    let formula = tap_show(bare_path, "main", "Formula/mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    // The root path must NOT exist.
    let root = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["cat-file", "-e", "main:mytool.rb"])
                .current_dir(bare_path);
            cmd
        },
        "git",
    )
    .status;
    assert!(
        !root.success(),
        "formula must live under Formula/, not the tap root"
    );
    drop(bare);
}

/// Non-default push branch: `repository.branch` routes the commit onto a
/// branch other than the tap's seeded default. Asserts the formula landed
/// on that branch ref.
#[test]
fn publish_to_homebrew_pushes_to_configured_branch() {
    let (bare_url, bare) = make_bare_tap("trunk");
    let hb = hb_cfg_local(&bare_url, "trunk");
    let mut ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    assert!(pushed);
    let bare_path = Path::new(&bare_url);
    let formula = tap_show(bare_path, "trunk", "mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    drop(bare);
}

/// A custom `commit_msg_template` renders into the actual tap commit
/// subject. Pins that the template (not a hard-coded string) drives the
/// landed commit message. `render_commit_msg` registers the formula name
/// as `ProjectName` (it is invoked with `ident.formula_name`) and the
/// version as `Version`; the Go-style leading dots are stripped by the
/// template preprocessor before Tera renders, so `.ProjectName` /
/// `.Version` resolve to those registered vars. (`.Name` is NOT a
/// registered var — using it would error-render and silently fall back
/// to the default message.)
#[test]
fn publish_to_homebrew_renders_custom_commit_message() {
    let (bare_url, bare) = make_bare_tap("main");
    let mut hb = hb_cfg_local(&bare_url, "main");
    hb.commit_msg_template = Some("brew: {{ .ProjectName }} bumped to {{ .Version }}".to_string());
    let mut ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    let subject = git_stdout(Path::new(&bare_url), &["log", "-1", "--pretty=%s", "main"]);
    assert_eq!(
        subject, "brew: mytool bumped to 1.2.3",
        "the custom commit_msg_template must drive the landed commit subject; \
             ProjectName = the formula name, Version = the release version"
    );
    drop(bare);
}

/// Idempotent re-publish: running the publisher twice against the same
/// tap (identical formula content) lands one commit the first time
/// (Ok(true)) and a no-op the second time (Ok(false)) — the
/// commit-and-push helper detects the unchanged tree and skips.
#[test]
fn publish_to_homebrew_second_run_is_noop() {
    let (bare_url, bare) = make_bare_tap("main");
    let hb = hb_cfg_local(&bare_url, "main");
    let make_ctx = || {
        single_crate_ctx(
            hb.clone(),
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        )
    };
    let mut ctx1 = make_ctx();
    assert!(
        publish_to_homebrew(&mut ctx1, "mytool", &quiet_log()).expect("first publish"),
        "first publish must push"
    );
    let mut ctx2 = make_ctx();
    assert!(
        !publish_to_homebrew(&mut ctx2, "mytool", &quiet_log()).expect("second publish"),
        "second publish of identical content must be a no-op (Ok(false))"
    );
    drop(bare);
}

/// Workspace lockstep mode: the crate lives only under
/// `config.workspaces[].crates` (no top-level entry). The publisher must
/// resolve it via the workspace fallthrough and still land the formula on
/// the tap — proving per-crate publish is not single-crate-only.
#[test]
fn publish_to_homebrew_workspace_crate_lands_formula() {
    let (bare_url, bare) = make_bare_tap("main");
    let hb = hb_cfg_local(&bare_url, "main");
    let config = Config {
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig {
                    homebrew: Some(hb),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // No tag → Version is empty; the workspace lockstep path still renders
    // (formula `version ""`), proving config resolution, not version math.
    ctx.artifacts.add(archive(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        "s",
    ));
    let pushed =
        publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("workspace publish ok");
    assert!(pushed, "workspace-only crate must still push the formula");
    let formula = tap_show(Path::new(&bare_url), "main", "mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    assert!(formula.contains("https://e/x.tar.gz"), "{formula}");
    drop(bare);
}

/// Workspace per-crate mode: two crates each carry their OWN homebrew
/// block pointing at distinct taps; publishing each lands ITS formula on
/// ITS tap with ITS own formula name. Proves per-crate config resolution
/// + per-crate name rendering, not a shared/last-writer-wins config.
#[test]
fn publish_to_homebrew_workspace_per_crate_distinct_taps() {
    let (bare_a, holder_a) = make_bare_tap("main");
    let (bare_b, holder_b) = make_bare_tap("main");
    let mut hb_a = hb_cfg_local(&bare_a, "main");
    hb_a.name = Some("alpha".to_string());
    let mut hb_b = hb_cfg_local(&bare_b, "main");
    hb_b.name = Some("beta".to_string());

    let crate_with = |name: &str, hb: HomebrewConfig| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            homebrew: Some(hb),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = Config {
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![crate_with("crate-a", hb_a), crate_with("crate-b", hb_b)],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // Each crate gets its own archive (artifact crate_name must match).
    let mut art_a = archive("x86_64-unknown-linux-gnu", "https://e/a.tar.gz", "sa");
    art_a.crate_name = "crate-a".to_string();
    let mut art_b = archive("x86_64-unknown-linux-gnu", "https://e/b.tar.gz", "sb");
    art_b.crate_name = "crate-b".to_string();
    ctx.artifacts.add(art_a);
    ctx.artifacts.add(art_b);

    assert!(publish_to_homebrew(&mut ctx, "crate-a", &quiet_log()).expect("publish a"));
    assert!(publish_to_homebrew(&mut ctx, "crate-b", &quiet_log()).expect("publish b"));

    // crate-a's tap carries alpha.rb with crate-a's url; crate-b's tap
    // carries beta.rb with crate-b's url. No cross-contamination.
    let fa = tap_show(Path::new(&bare_a), "main", "alpha.rb");
    assert!(fa.contains("class Alpha < Formula"), "{fa}");
    assert!(fa.contains("https://e/a.tar.gz"), "{fa}");
    let fb = tap_show(Path::new(&bare_b), "main", "beta.rb");
    assert!(fb.contains("class Beta < Formula"), "{fb}");
    assert!(fb.contains("https://e/b.tar.gz"), "{fb}");
    drop(holder_a);
    drop(holder_b);
}

/// Same-tap cask co-publish: with a `cask:` block + a darwin DiskImage
/// artifact, the publisher writes the cask alongside the formula into the
/// same clone and the single commit covers BOTH files. Asserts both
/// `mytool.rb` (formula) and `Casks/<cask>.rb` landed on the tap, and the
/// commit subject reflects the formula+cask kind.
#[test]
fn publish_to_homebrew_co_publishes_cask_into_same_tap() {
    let (bare_url, bare) = make_bare_tap("main");
    let mut hb = hb_cfg_local(&bare_url, "main");
    hb.cask = Some(HomebrewCaskConfig {
        name: Some("mytool-cask".to_string()),
        url: Some(HomebrewCaskURL {
            template: Some("https://e/{{ .Version }}/mytool.dmg".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let mut dmg_meta = HashMap::new();
    dmg_meta.insert("url".to_string(), "https://e/mytool.dmg".to_string());
    dmg_meta.insert("sha256".to_string(), "dmgsha".to_string());
    let dmg = Artifact {
        kind: ArtifactKind::DiskImage,
        path: std::path::PathBuf::from("/tmp/mytool.dmg"),
        name: "mytool.dmg".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "mytool".to_string(),
        metadata: dmg_meta,
        size: None,
    };
    let mut ctx = single_crate_ctx(
        hb,
        vec![
            archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
            dmg,
        ],
    );
    let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    assert!(pushed);
    let bare_path = Path::new(&bare_url);
    // Formula at root.
    let formula = tap_show(bare_path, "main", "mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    // Cask under the default Casks/ dir.
    let cask = tap_show(bare_path, "main", "Casks/mytool-cask.rb");
    assert!(cask.contains("cask \"mytool-cask\""), "{cask}");
    // Single commit, formula+cask kind in the subject.
    let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
    assert!(
        subject.contains("cask"),
        "commit subject must reflect the formula+cask kind; got: {subject}"
    );
    drop(bare);
}

/// PR path: with `pull_request.enabled = true` (same-repo), the publisher
/// still commits+pushes the formula to the tap AND attempts a PR. The
/// formula push (the local effect) must land regardless of the PR outcome.
///
/// Hermetic by construction: a failing `gh` stub forces the PR transport's
/// `gh_is_available()` probe to false, and no token is configured, so the
/// PR submission resolves to the `NoneAvailable` fallback IN-PROCESS — it
/// never issues a live `gh pr create` / GitHub API call against
/// `myorg/homebrew-tap`. Holds the `PathGuard` for the whole test and is
/// `#[serial(path_env)]` because it mutates process `PATH` (the shared
/// `path_env` group serializes it against the `util/pr.rs` and
/// winget/scoop/krew gh-stub tests crate-wide).
#[test]
#[serial(path_env)]
fn publish_to_homebrew_pr_enabled_still_pushes_formula() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = make_bare_tap("main");
    let mut hb = hb_cfg_local(&bare_url, "main");
    if let Some(repo) = hb.repository.as_mut() {
        repo.pull_request = Some(PullRequestConfig {
            enabled: Some(true),
            ..Default::default()
        });
        // No token configured: with `gh` stubbed absent too, the PR
        // transport has neither path and resolves to NoneAvailable
        // in-process (no network), yet the push already happened.
    }
    let mut ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    assert!(
        pushed,
        "formula push must land even when PR submission is attempted"
    );
    let formula = tap_show(Path::new(&bare_url), "main", "mytool.rb");
    assert!(formula.contains("class Mytool < Formula"), "{formula}");
    drop(bare);
}

/// Clone failure surfaces as an `Err`: pointing `git.url` at a path that
/// is not a git repo makes the clone fail; the publisher must propagate
/// the error (the tap was never touched), not silently report success.
#[test]
fn publish_to_homebrew_clone_failure_errors() {
    let bogus = tempfile::tempdir().expect("bogus dir");
    let bogus_url = bogus.path().to_string_lossy().into_owned();
    let hb = hb_cfg_local(&bogus_url, "main");
    let mut ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let err = publish_to_homebrew(&mut ctx, "mytool", &quiet_log())
        .expect_err("cloning a non-repo path must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("homebrew"),
        "error must name the publisher; got: {msg}"
    );
    drop(bogus);
}

// ===================================================================
// completions / manpages / livecheck / dual-license — the homebrew-core
// citizen fields the formula previously lacked (the cask already had).
// Validated against the real ripgrep/fd/bat exemplar idioms.
// ===================================================================

/// Single-crate mode: prebuilt completion files + a manpage render as
/// `bash_completion.install` / `zsh_completion.install` /
/// `fish_completion.install` / `man1.install` lines INSIDE the install
/// block — exactly the idiom ripgrep/fd/bat ship.
#[test]
fn render_formula_emits_completion_and_manpage_installs_single_crate() {
    let hb = HomebrewConfig {
        completions: Some(HomebrewCaskCompletions {
            bash: Some("completions/mytool.bash".to_string()),
            zsh: Some("completions/_mytool".to_string()),
            fish: Some("completions/mytool.fish".to_string()),
        }),
        manpages: Some(vec!["man/mytool.1".to_string()]),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("bin.install \"mytool\""), "{body}");
    assert!(
        body.contains("bash_completion.install \"completions/mytool.bash\""),
        "{body}"
    );
    assert!(
        body.contains("zsh_completion.install \"completions/_mytool\""),
        "{body}"
    );
    assert!(
        body.contains("fish_completion.install \"completions/mytool.fish\""),
        "{body}"
    );
    assert!(body.contains("man1.install \"man/mytool.1\""), "{body}");
}

/// A manpage path ending in `.8` routes to `man8.install`, not `man1`.
#[test]
fn render_formula_routes_manpage_to_numbered_section() {
    let hb = HomebrewConfig {
        manpages: Some(vec!["man/mytool.8".to_string()]),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("man8.install \"man/mytool.8\""), "{body}");
}

/// `generate_completions_from_executable` (the modern homebrew-core idiom)
/// renders inside the install block when configured.
#[test]
fn render_formula_emits_generate_completions_from_executable() {
    let hb = HomebrewConfig {
        generate_completions_from_executable: Some(HomebrewCaskGeneratedCompletions {
            executable: Some("bin/mytool".to_string()),
            args: Some(vec!["completions".to_string()]),
            shells: Some(vec![
                "bash".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(
        body.contains("generate_completions_from_executable \"bin/mytool\", \"completions\""),
        "{body}"
    );
    assert!(body.contains("shells: [:bash, :zsh, :fish]"), "{body}");
}

/// A user-supplied `install:` block OWNS the install body — anodizer must
/// NOT append auto-completion/man lines (no double-emit).
#[test]
fn render_formula_custom_install_does_not_append_completions() {
    let hb = HomebrewConfig {
        install: Some("bin.install \"mytool\"".to_string()),
        completions: Some(HomebrewCaskCompletions {
            bash: Some("c.bash".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(
        !body.contains("bash_completion.install"),
        "custom install owns the block; got:\n{body}"
    );
}

/// Default livecheck: a binary tap formula with NO livecheck config emits
/// `livecheck do\n  skip "Auto-generated on release."\nend`, mirroring the
/// cask (the archive URL/sha change every release).
#[test]
fn render_formula_emits_default_livecheck_skip() {
    let hb = HomebrewConfig::default();
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("livecheck do"), "{body}");
    assert!(
        body.contains("skip \"Auto-generated on release.\""),
        "{body}"
    );
}

/// Active livecheck: opting in with `skip: false` + a strategy renders a
/// `url :stable` / `strategy :github_latest` block, matching ripgrep.
#[test]
fn render_formula_emits_active_livecheck_strategy() {
    let hb = HomebrewConfig {
        livecheck: Some(HomebrewLivecheck {
            skip: Some(false),
            strategy: Some("github_latest".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("livecheck do"), "{body}");
    assert!(body.contains("url :stable"), "{body}");
    assert!(body.contains("strategy :github_latest"), "{body}");
    assert!(
        !body.contains("skip \"Auto-generated"),
        "active livecheck must NOT skip; got:\n{body}"
    );
}

/// `skip: false` with NO strategy/url/regex is a no-op opt-in: an empty
/// `livecheck do…end` is invalid Ruby, so the renderer falls back to `skip`
/// (and warns — the warning is surfaced, not asserted here, since it goes to
/// stderr). The rendered formula must still carry a valid `skip` block.
#[test]
fn render_formula_livecheck_skip_false_without_strategy_falls_back_to_skip() {
    let hb = HomebrewConfig {
        livecheck: Some(HomebrewLivecheck {
            skip: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("livecheck do"), "{body}");
    assert!(
        body.contains("skip \"Auto-generated on release.\""),
        "no-op opt-in must fall back to a valid skip block; got:\n{body}"
    );
}

/// Dual-license SPDX (`Apache-2.0 OR MIT`) — the Rust-CLI norm — renders as
/// `license any_of: ["Apache-2.0", "MIT"]`, NOT an invalid bare string.
#[test]
fn render_formula_dual_license_renders_any_of_single_crate() {
    let hb = HomebrewConfig {
        license: Some("Apache-2.0 OR MIT".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(
        body.contains("license any_of: [\"Apache-2.0\", \"MIT\"]"),
        "{body}"
    );
    assert!(
        !body.contains("license \"Apache-2.0 OR MIT\""),
        "must not emit the invalid bare-string form; got:\n{body}"
    );
}

/// AND dual-license renders `license all_of: [...]`.
#[test]
fn render_formula_and_license_renders_all_of() {
    let hb = HomebrewConfig {
        license: Some("Apache-2.0 AND MIT".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(
        body.contains("license all_of: [\"Apache-2.0\", \"MIT\"]"),
        "{body}"
    );
}

/// A single-id license still renders the plain `license "MIT"` form.
#[test]
fn render_formula_single_license_renders_plain_string() {
    let hb = HomebrewConfig {
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let ctx = single_crate_ctx(
        hb,
        vec![archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        )],
    );
    let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
        .expect("render")
        .expect("not skipped")
        .formula;
    assert!(body.contains("license \"MIT\""), "{body}");
    assert!(!body.contains("any_of"), "{body}");
}

/// Workspace per-crate mode: two crates carry DISTINCT dual licenses +
/// distinct completion sets. Each formula must render ITS OWN license
/// `any_of:` and ITS OWN completion installs — proving per-crate resolution
/// of the new fields, not last-writer-wins.
#[test]
fn render_formula_per_crate_distinct_license_and_completions() {
    let hb_a = HomebrewConfig {
        name: Some("alpha".to_string()),
        license: Some("Apache-2.0 OR MIT".to_string()),
        completions: Some(HomebrewCaskCompletions {
            bash: Some("a.bash".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let hb_b = HomebrewConfig {
        name: Some("beta".to_string()),
        license: Some("BSD-3-Clause".to_string()),
        completions: Some(HomebrewCaskCompletions {
            zsh: Some("_beta".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let crate_with = |name: &str, hb: HomebrewConfig| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            homebrew: Some(hb),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = Config {
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![crate_with("crate-a", hb_a), crate_with("crate-b", hb_b)],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut art_a = archive("x86_64-unknown-linux-gnu", "https://e/a.tar.gz", "sa");
    art_a.crate_name = "crate-a".to_string();
    let mut art_b = archive("x86_64-unknown-linux-gnu", "https://e/b.tar.gz", "sb");
    art_b.crate_name = "crate-b".to_string();
    ctx.artifacts.add(art_a);
    ctx.artifacts.add(art_b);

    let body_a = render_homebrew_formula_for_crate(&ctx, "crate-a", &quiet_log())
        .expect("render a")
        .expect("not skipped")
        .formula;
    let body_b = render_homebrew_formula_for_crate(&ctx, "crate-b", &quiet_log())
        .expect("render b")
        .expect("not skipped")
        .formula;

    // crate-a: dual license any_of + bash completion only.
    assert!(
        body_a.contains("license any_of: [\"Apache-2.0\", \"MIT\"]"),
        "a:\n{body_a}"
    );
    assert!(
        body_a.contains("bash_completion.install \"a.bash\""),
        "a:\n{body_a}"
    );
    assert!(
        !body_a.contains("_beta"),
        "no cross-contamination; a:\n{body_a}"
    );

    // crate-b: single license + zsh completion only.
    assert!(body_b.contains("license \"BSD-3-Clause\""), "b:\n{body_b}");
    assert!(!body_b.contains("any_of"), "b:\n{body_b}");
    assert!(
        body_b.contains("zsh_completion.install \"_beta\""),
        "b:\n{body_b}"
    );
    assert!(
        !body_b.contains("a.bash"),
        "no cross-contamination; b:\n{body_b}"
    );
}
