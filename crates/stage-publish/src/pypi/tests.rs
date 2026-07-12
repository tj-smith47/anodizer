//! Tests for the PyPI publisher: platform-tag derivation, wheel assembly
//! (content + determinism), upload form protocol, skip_existing semantics,
//! preflight validation, and the config-mode axis.

use std::io::Read as _;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig, PypiConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use anodizer_core::{PreflightCheck, Publisher};

use super::publisher::{PypiPublisher, publish_to_pypi, resolve_token, version_probe};
use super::upload::{FileType, UploadOutcome, is_duplicate_rejection, upload_file};
use super::wheel::{BinaryTraits, WheelSpec, build_wheel, inspect_binary, platform_tag};

fn traits(
    glibc: Option<(u64, u64)>,
    macos_min: Option<(u16, u16)>,
    universal: bool,
) -> BinaryTraits {
    BinaryTraits {
        glibc,
        macos_min,
        universal,
    }
}

// -----------------------------------------------------------------------------
// Platform-tag derivation
// -----------------------------------------------------------------------------

#[test]
fn gnu_targets_tag_manylinux_from_glibc_floor() {
    let t = traits(Some((2, 28)), None, false);
    assert_eq!(
        platform_tag("x86_64-unknown-linux-gnu", &t).unwrap(),
        "manylinux_2_28_x86_64"
    );
    assert_eq!(
        platform_tag(
            "aarch64-unknown-linux-gnu",
            &traits(Some((2, 17)), None, false)
        )
        .unwrap(),
        "manylinux_2_17_aarch64"
    );
    assert_eq!(
        platform_tag(
            "i686-unknown-linux-gnu",
            &traits(Some((2, 36)), None, false)
        )
        .unwrap(),
        "manylinux_2_36_i686"
    );
}

#[test]
fn gnu_target_without_glibc_requirement_errors() {
    let err = platform_tag("x86_64-unknown-linux-gnu", &traits(None, None, false)).unwrap_err();
    assert!(err.to_string().contains("GLIBC"), "{err:#}");
}

#[test]
fn musl_targets_tag_musllinux_1_2() {
    assert_eq!(
        platform_tag("x86_64-unknown-linux-musl", &traits(None, None, false)).unwrap(),
        "musllinux_1_2_x86_64"
    );
    assert_eq!(
        platform_tag("aarch64-unknown-linux-musl", &traits(None, None, false)).unwrap(),
        "musllinux_1_2_aarch64"
    );
}

#[test]
fn darwin_targets_tag_macosx_from_minos() {
    assert_eq!(
        platform_tag("x86_64-apple-darwin", &traits(None, Some((10, 13)), false)).unwrap(),
        "macosx_10_13_x86_64"
    );
    assert_eq!(
        platform_tag("aarch64-apple-darwin", &traits(None, Some((12, 0)), false)).unwrap(),
        "macosx_12_0_arm64"
    );
}

#[test]
fn darwin_minos_fallbacks_are_arch_appropriate() {
    // No load command: Intel falls back to 10.12, arm64 to 11.0 (first
    // arm64 macOS).
    assert_eq!(
        platform_tag("x86_64-apple-darwin", &traits(None, None, false)).unwrap(),
        "macosx_10_12_x86_64"
    );
    assert_eq!(
        platform_tag("aarch64-apple-darwin", &traits(None, None, false)).unwrap(),
        "macosx_11_0_arm64"
    );
}

#[test]
fn universal_binaries_tag_universal2() {
    assert_eq!(
        platform_tag("aarch64-apple-darwin", &traits(None, Some((11, 0)), true)).unwrap(),
        "macosx_11_0_universal2"
    );
    // Fallback with no minos load command still tags universal2 at the
    // arm64 floor (a universal binary always carries an arm64 slice).
    assert_eq!(
        platform_tag("x86_64-apple-darwin", &traits(None, None, true)).unwrap(),
        "macosx_11_0_universal2"
    );
}

#[test]
fn windows_targets_map_to_win_tags() {
    let t = traits(None, None, false);
    assert_eq!(
        platform_tag("x86_64-pc-windows-msvc", &t).unwrap(),
        "win_amd64"
    );
    assert_eq!(platform_tag("i686-pc-windows-msvc", &t).unwrap(), "win32");
    assert_eq!(
        platform_tag("aarch64-pc-windows-msvc", &t).unwrap(),
        "win_arm64"
    );
    assert_eq!(
        platform_tag("x86_64-pc-windows-gnu", &t).unwrap(),
        "win_amd64"
    );
}

#[test]
fn unmapped_targets_error() {
    let t = traits(None, None, false);
    assert!(platform_tag("wasm32-unknown-unknown", &t).is_err());
    assert!(platform_tag("riscv64gc-unknown-linux-gnu", &t).is_err());
}

#[test]
fn inspect_binary_degrades_on_non_object_bytes() {
    let t = inspect_binary(b"not an ELF or Mach-O", false).unwrap();
    assert!(t.glibc.is_none());
    assert!(t.macos_min.is_none());
}

// -----------------------------------------------------------------------------
// Wheel assembly
// -----------------------------------------------------------------------------

fn spec(tag: &str) -> WheelSpec {
    WheelSpec {
        name: "My-Tool".to_string(),
        version: "1.2.3".to_string(),
        platform_tag: tag.to_string(),
        bin_name: "my-tool".to_string(),
        summary: Some("A tool".to_string()),
        description: Some("Long description".to_string()),
        license: Some("MIT".to_string()),
        homepage: Some("https://example.com".to_string()),
        requires_python: Some(">=3.7".to_string()),
        keywords: vec!["cli".to_string(), "rust".to_string()],
        classifiers: vec!["Programming Language :: Rust".to_string()],
    }
}

#[test]
fn wheel_filename_escapes_distribution_name() {
    assert_eq!(
        spec("musllinux_1_2_x86_64").filename(),
        "My_Tool-1.2.3-py3-none-musllinux_1_2_x86_64.whl"
    );
}

fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> String {
    let mut f = zip.by_name(name).unwrap_or_else(|_| panic!("entry {name}"));
    let mut s = String::new();
    f.read_to_string(&mut s).expect("read entry");
    s
}

#[test]
fn wheel_contents_carry_metadata_wheel_and_record() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let s = spec("manylinux_2_28_x86_64");
    let path = build_wheel(
        &s,
        b"#!fake-binary",
        tmp.path(),
        Some(1700000000),
        "0.0.0-test",
    )
    .expect("build wheel");
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).expect("open")).expect("zip");

    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).expect("by_index").name().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "My_Tool-1.2.3.data/scripts/my-tool",
            "My_Tool-1.2.3.dist-info/METADATA",
            "My_Tool-1.2.3.dist-info/RECORD",
            "My_Tool-1.2.3.dist-info/WHEEL",
        ],
        "entries sorted by path"
    );

    // The executable carries unix mode 0755 in the zip external attrs.
    let script = zip
        .by_name("My_Tool-1.2.3.data/scripts/my-tool")
        .expect("script entry");
    assert_eq!(script.unix_mode().map(|m| m & 0o777), Some(0o755));
    drop(script);

    let metadata = read_entry(&mut zip, "My_Tool-1.2.3.dist-info/METADATA");
    assert!(
        metadata.starts_with("Metadata-Version: 2.1\n"),
        "{metadata}"
    );
    assert!(metadata.contains("Name: My-Tool\n"), "{metadata}");
    assert!(metadata.contains("Version: 1.2.3\n"), "{metadata}");
    assert!(metadata.contains("Summary: A tool\n"), "{metadata}");
    assert!(metadata.contains("License: MIT\n"), "{metadata}");
    assert!(metadata.contains("Requires-Python: >=3.7\n"), "{metadata}");
    assert!(
        metadata.contains("Classifier: Programming Language :: Rust\n"),
        "{metadata}"
    );
    assert!(
        metadata.contains("Project-URL: Homepage, https://example.com\n"),
        "{metadata}"
    );
    assert!(metadata.contains("Keywords: cli,rust\n"), "{metadata}");
    assert!(metadata.ends_with("\nLong description\n"), "{metadata}");

    let wheel_file = read_entry(&mut zip, "My_Tool-1.2.3.dist-info/WHEEL");
    assert_eq!(
        wheel_file,
        "Wheel-Version: 1.0\nGenerator: anodizer 0.0.0-test\nRoot-Is-Purelib: false\n\
         Tag: py3-none-manylinux_2_28_x86_64\n"
    );

    let record = read_entry(&mut zip, "My_Tool-1.2.3.dist-info/RECORD");
    let rows: Vec<&str> = record.trim_end().lines().collect();
    assert_eq!(rows.len(), 4, "{record}");
    // The binary row pins the urlsafe-b64 (no pad) sha256 + size.
    use base64::Engine as _;
    use sha2::Digest as _;
    let bin_hash = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(b"#!fake-binary"));
    assert_eq!(
        rows[0],
        format!(
            "My_Tool-1.2.3.data/scripts/my-tool,sha256={},{}",
            bin_hash,
            b"#!fake-binary".len()
        )
    );
    // RECORD's own row is last with empty hash/size.
    assert_eq!(rows[3], "My_Tool-1.2.3.dist-info/RECORD,,");
}

#[test]
fn wheel_bytes_are_deterministic() {
    let tmp_a = tempfile::TempDir::new().expect("tmp a");
    let tmp_b = tempfile::TempDir::new().expect("tmp b");
    let s = spec("musllinux_1_2_x86_64");
    let a = build_wheel(&s, b"same-bytes", tmp_a.path(), Some(1700000000), "1.0.0").expect("a");
    let b = build_wheel(&s, b"same-bytes", tmp_b.path(), Some(1700000000), "1.0.0").expect("b");
    assert_eq!(
        std::fs::read(a).expect("read a"),
        std::fs::read(b).expect("read b"),
        "two builds of the same inputs must be byte-identical"
    );
}

// -----------------------------------------------------------------------------
// Upload protocol
// -----------------------------------------------------------------------------

fn upload_ctx_pieces(
    tmp: &std::path::Path,
) -> (std::path::PathBuf, WheelSpec, reqwest::blocking::Client) {
    let s = spec("musllinux_1_2_x86_64");
    let path = build_wheel(&s, b"#!fake-binary", tmp, Some(1700000000), "1.0.0").expect("wheel");
    let client =
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(5)).expect("client");
    (path, s, client)
}

#[test]
fn upload_sends_twine_protocol_form_fields() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (path, s, client) = upload_ctx_pieces(tmp.path());
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: Some(1),
    }]);
    let out = upload_file(
        &client,
        &format!("http://{addr}/legacy/"),
        "pypi-AgToken",
        "my-tool",
        &s,
        FileType::Wheel,
        &path,
        true,
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        None,
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("upload");
    assert!(matches!(out, UploadOutcome::Uploaded { .. }));

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1);
    let req = &entries[0];
    // Basic auth: __token__:<token>.
    use base64::Engine as _;
    let expect_auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("__token__:pypi-AgToken")
    );
    assert_eq!(req.header("authorization"), Some(expect_auth.as_str()));
    for needle in [
        "name=\":action\"\r\n\r\nfile_upload",
        "name=\"protocol_version\"\r\n\r\n1",
        "name=\"name\"\r\n\r\nmy-tool",
        "name=\"version\"\r\n\r\n1.2.3",
        "name=\"filetype\"\r\n\r\nbdist_wheel",
        "name=\"pyversion\"\r\n\r\npy3",
        "name=\"metadata_version\"\r\n\r\n2.1",
        "name=\"summary\"\r\n\r\nA tool",
        "name=\"requires_python\"\r\n\r\n>=3.7",
        "name=\"classifiers\"\r\n\r\nProgramming Language :: Rust",
        "name=\"sha256_digest\"",
        "filename=\"My_Tool-1.2.3-py3-none-musllinux_1_2_x86_64.whl\"",
    ] {
        assert!(req.body.contains(needle), "missing {needle:?} in body");
    }
}

#[test]
fn duplicate_rejection_shapes_are_matched_generously() {
    assert!(is_duplicate_rejection(409, ""));
    assert!(is_duplicate_rejection(
        400,
        "File already exists. See https://pypi.org/help/#file-name-reuse"
    ));
    assert!(is_duplicate_rejection(403, "this file already exists"));
    assert!(!is_duplicate_rejection(400, "invalid classifier"));
    assert!(!is_duplicate_rejection(500, "already exists"));
}

#[test]
fn skip_existing_folds_duplicate_400_into_idempotent_skip() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (path, s, client) = upload_ctx_pieces(tmp.path());
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 400 Bad Request\r\nContent-Length: 20\r\n\r\nFile already exists.",
        times: Some(1),
    }]);
    let out = upload_file(
        &client,
        &format!("http://{addr}/legacy/"),
        "tok",
        "my-tool",
        &s,
        FileType::Wheel,
        &path,
        true,
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        None,
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("skip-existing upload");
    assert!(matches!(out, UploadOutcome::SkippedExisting { .. }));
}

#[test]
fn duplicate_is_a_hard_error_when_skip_existing_false() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (path, s, client) = upload_ctx_pieces(tmp.path());
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 400 Bad Request\r\nContent-Length: 20\r\n\r\nFile already exists.",
        times: Some(1),
    }]);
    let err = upload_file(
        &client,
        &format!("http://{addr}/legacy/"),
        "tok",
        "my-tool",
        &s,
        FileType::Wheel,
        &path,
        false,
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        None,
        anodizer_core::test_helpers::test_logger(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("skip_existing"), "{err:#}");
}

// -----------------------------------------------------------------------------
// Publish orchestration (per config mode)
// -----------------------------------------------------------------------------

fn demo_crate(name: &str, path: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }
}

fn add_binary(
    ctx: &mut anodizer_core::context::Context,
    dir: &std::path::Path,
    target: &str,
    crate_name: &str,
    bin_name: &str,
) {
    let path = dir.join(format!("{bin_name}-{target}"));
    std::fs::write(&path, b"#!fake-binary").expect("write binary");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        path,
        name: bin_name.to_string(),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
}

fn publish_ctx(
    tmp: &std::path::Path,
    crates: Vec<CrateConfig>,
    cfg: PypiConfig,
) -> anodizer_core::context::Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(crates)
        .dist(tmp.join("dist"))
        .env("PYPI_TOKEN", "tok")
        .build();
    ctx.config.pypis = Some(vec![cfg]);
    ctx
}

/// End-to-end publish against a scripted index for one config shape;
/// returns the recorded snapshots and the responder's request log.
fn run_publish_end_to_end(
    crates: Vec<CrateConfig>,
    cfg_for: impl FnOnce(String) -> PypiConfig,
) -> (
    Vec<anodizer_core::publish_evidence::PypiFileSnapshot>,
    std::sync::Arc<
        std::sync::Mutex<Vec<anodizer_core::test_helpers::scripted_responder::RequestLog>>,
    >,
) {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let cfg = cfg_for(format!("http://{addr}/legacy/"));
    let mut ctx = publish_ctx(tmp.path(), crates, cfg);
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("publish");
    (files, log)
}

#[test]
fn publishes_wheel_in_single_crate_mode() {
    let (files, log) = run_publish_end_to_end(vec![demo_crate("demo", ".")], |repo| PypiConfig {
        repository: Some(repo),
        ..Default::default()
    });
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].filename,
        "demo-1.2.3-py3-none-musllinux_1_2_x86_64.whl"
    );
    assert_eq!(files[0].platform_tag, "musllinux_1_2_x86_64");
    assert!(!files[0].skipped_existing);
    assert_eq!(files[0].sha256.len(), 64, "hex sha256");
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn publishes_wheel_in_lockstep_workspace_mode() {
    // Two lockstep crates; the entry names the project explicitly and the
    // first crate's binary ships.
    let (files, _log) = run_publish_end_to_end(
        vec![demo_crate("demo", "."), demo_crate("other", "other")],
        |repo| PypiConfig {
            name: Some("demo".into()),
            ids: Some(vec!["demo".into()]),
            repository: Some(repo),
            ..Default::default()
        },
    );
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].filename,
        "demo-1.2.3-py3-none-musllinux_1_2_x86_64.whl"
    );
}

#[test]
fn ids_filter_scopes_binaries_per_crate() {
    // Two crates' binaries in the artifact set; `ids: [demo]` keeps the
    // per-crate entry scoped to its own crate — the workspace per-crate
    // pattern.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let mut ctx = publish_ctx(
        tmp.path(),
        vec![demo_crate("demo", "."), demo_crate("other", "other")],
        PypiConfig {
            ids: Some(vec!["demo".into()]),
            repository: Some(format!("http://{addr}/legacy/")),
            ..Default::default()
        },
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "demo",
        "demo",
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "aarch64-unknown-linux-musl",
        "other",
        "other",
    );
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("publish");
    assert_eq!(files.len(), 1, "only the demo crate's binary is selected");
    assert!(files[0].filename.starts_with("demo-1.2.3"));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn unmappable_prerelease_version_errors() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3-nightly.20260712")
        .crates(vec![demo_crate("demo", ".")])
        .dist(tmp.path().join("dist"))
        .env("PYPI_TOKEN", "tok")
        .build();
    ctx.config.pypis = Some(vec![PypiConfig::default()]);
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    let err = publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).unwrap_err();
    assert!(err.to_string().contains("PEP 440"), "{err:#}");
    assert!(files.is_empty());
}

#[test]
fn sdist_without_manifest_errors() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = publish_ctx(
        tmp.path(),
        vec![demo_crate("demo", ".")],
        PypiConfig {
            sdist: true,
            ..Default::default()
        },
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    let err = publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).unwrap_err();
    assert!(err.to_string().contains("sdist_manifest"), "{err:#}");
}

#[test]
fn dry_run_makes_no_requests() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .dist(tmp.path().join("dist"))
        .dry_run(true)
        .build();
    ctx.config.pypis = Some(vec![PypiConfig {
        repository: Some(format!("http://{addr}/legacy/")),
        ..Default::default()
    }]);
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("dry-run publish");
    assert!(files.is_empty(), "dry-run uploads nothing");
    assert_eq!(log.lock().unwrap().len(), 0, "dry-run makes no requests");
}

// -----------------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_pypi_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
pypis:
  - name: my-tool
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse minimal pypis");
    let entries = cfg.pypis.expect("pypis set");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name.as_deref(), Some("my-tool"));
    assert!(entries[0].skip_existing, "skip_existing defaults true");
    assert!(!entries[0].sdist, "sdist defaults false");
}

#[test]
fn parse_full_pypi_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
pypis:
  - id: main
    ids: [demo]
    name: My.Tool
    sdist: true
    sdist_manifest: "pypi/"
    repository: "https://test.pypi.org/legacy/"
    skip_existing: false
    requires_python: ">=3.8"
    summary: "A demo"
    description: "Long form"
    homepage: "https://example.com"
    license: MIT
    keywords: [cli]
    classifiers: ["Programming Language :: Rust"]
    token: "{{ .Env.MY_PYPI }}"
    skip: false
    required: false
    retain_on_rollback: true
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse full pypis");
    let e = &cfg.pypis.expect("pypis set")[0];
    assert_eq!(e.id.as_deref(), Some("main"));
    assert!(e.sdist);
    assert_eq!(e.sdist_manifest.as_deref(), Some("pypi/"));
    assert!(!e.skip_existing);
    assert_eq!(e.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(e.required, Some(false));
    assert_eq!(e.retain_on_rollback, Some(true));
}

// -----------------------------------------------------------------------------
// Publisher contract
// -----------------------------------------------------------------------------

#[test]
fn pypi_publisher_classification() {
    let p = PypiPublisher::new();
    assert_eq!(p.name(), "pypi");
    assert_eq!(p.group(), anodizer_core::PublisherGroup::Manager);
    assert!(p.required());
    assert!(p.skips_on_nightly());
    assert_eq!(p.rollback_scope_needed(), None);
}

#[test]
fn requirements_use_token_ladder_and_maturin_when_sdist() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    ctx.config.pypis = Some(vec![PypiConfig {
        sdist: true,
        sdist_manifest: Some("pypi/".into()),
        ..Default::default()
    }]);
    let reqs = PypiPublisher::new().requirements(&ctx);
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::Tool { name } if name == "maturin"
        )),
        "sdist demands maturin: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::EnvAnyOf { vars }
                if vars == &vec!["PYPI_TOKEN".to_string(), "MATURIN_PYPI_TOKEN".to_string()]
        )),
        "token ladder is an any-of: {reqs:?}"
    );

    // Without sdist, maturin is not demanded.
    ctx.config.pypis = Some(vec![PypiConfig::default()]);
    let reqs = PypiPublisher::new().requirements(&ctx);
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r, anodizer_core::EnvRequirement::Tool { .. })),
        "{reqs:?}"
    );
}

#[test]
fn token_resolution_ladder() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .env("MATURIN_PYPI_TOKEN", "maturin-tok")
        .build();
    // Fallback reaches MATURIN_PYPI_TOKEN when PYPI_TOKEN is unset.
    assert_eq!(
        resolve_token(&ctx, &PypiConfig::default()).expect("resolve"),
        "maturin-tok"
    );
    // PYPI_TOKEN wins over MATURIN_PYPI_TOKEN.
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("PYPI_TOKEN", "pypi-tok")
            .with("MATURIN_PYPI_TOKEN", "maturin-tok"),
    );
    assert_eq!(
        resolve_token(&ctx, &PypiConfig::default()).expect("resolve"),
        "pypi-tok"
    );
    // Configured token wins over both.
    let cfg = PypiConfig {
        token: Some("inline-tok".into()),
        ..Default::default()
    };
    assert_eq!(resolve_token(&ctx, &cfg).expect("resolve"), "inline-tok");
}

// -----------------------------------------------------------------------------
// Preflight
// -----------------------------------------------------------------------------

#[test]
fn preflight_blocks_on_illegal_project_name() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    ctx.config.pypis = Some(vec![PypiConfig {
        name: Some("-bad-name-".into()),
        ..Default::default()
    }]);
    match PypiPublisher::new().preflight(&ctx).expect("preflight") {
        PreflightCheck::Blocker(m) => assert!(m.contains("not a legal PyPI name"), "{m}"),
        other => panic!("expected Blocker, got {other:?}"),
    }
}

#[test]
fn preflight_blocks_on_unmappable_version() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3-nightly.1")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    ctx.config.pypis = Some(vec![PypiConfig {
        // A repository that version_probe maps to the /simple/ probe would
        // hit the network; an invalid URL keeps this test offline.
        repository: Some("not a url".into()),
        ..Default::default()
    }]);
    match PypiPublisher::new().preflight(&ctx).expect("preflight") {
        PreflightCheck::Blocker(m) => assert!(m.contains("PEP 440"), "{m}"),
        other => panic!("expected Blocker, got {other:?}"),
    }
}

#[test]
fn preflight_skips_deselected_entries() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    ctx.config.pypis = Some(vec![PypiConfig {
        name: Some("-bad-name-".into()),
        if_condition: Some("false".into()),
        ..Default::default()
    }]);
    assert!(matches!(
        PypiPublisher::new().preflight(&ctx).expect("preflight"),
        PreflightCheck::Pass
    ));
}

#[test]
fn version_probe_maps_known_hosts_to_json_api() {
    assert_eq!(
        version_probe("https://upload.pypi.org/legacy/", "my-tool", "1.2.3"),
        Some((
            "https://pypi.org/pypi/my-tool/1.2.3/json".to_string(),
            false
        ))
    );
    assert_eq!(
        version_probe("https://test.pypi.org/legacy/", "my-tool", "1.2.3"),
        Some((
            "https://test.pypi.org/pypi/my-tool/1.2.3/json".to_string(),
            false
        ))
    );
    assert_eq!(
        version_probe("https://pypi.example.com:8443/legacy/", "my-tool", "1.2.3"),
        Some((
            "https://pypi.example.com:8443/simple/my-tool/".to_string(),
            true
        ))
    );
    assert_eq!(version_probe("not a url", "my-tool", "1.2.3"), None);
}
