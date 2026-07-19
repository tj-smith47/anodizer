//! Tests for the PyPI publisher: platform-tag derivation, wheel assembly
//! (content + determinism), upload form protocol, skip_existing semantics,
//! preflight validation, and the config-mode axis.

use std::io::Read as _;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig, PypiConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use anodizer_core::{PreflightCheck, Publisher};

use super::publisher::{
    PypiPublisher, build_spec_base, publish_to_pypi, resolve_token, version_probe,
};
use super::upload::{FileType, UploadOutcome, is_duplicate_rejection, upload_file};
use super::wheel::{
    BinaryTraits, WheelSpec, build_wheel, inspect_binary, platform_tag, render_metadata,
};

fn traits(
    glibc: Option<(u64, u64)>,
    macos_min: Option<(u16, u16)>,
    universal: bool,
) -> BinaryTraits {
    // Healthy-binary traits: `macho: true` so darwin tag derivation exercises
    // the fallback path, not the not-a-Mach-O hard error (that case builds
    // BinaryTraits explicitly with `macho: false`). `elf: true` +
    // `dynamically_linked: true` model a healthy dynamic ELF, so a gnu triple
    // with `glibc: None` exercises the dynamic-missing-glibc hard error rather
    // than the fully-static path (which its own tests build explicitly).
    BinaryTraits {
        glibc,
        macos_min,
        macho: true,
        universal,
        elf: true,
        dynamically_linked: true,
    }
}

/// Traits for a fully-static gnu ELF: an ELF with no `PT_INTERP` and no glibc
/// requirement — anodizer's default linux-gnu build shape.
fn static_gnu_traits() -> BinaryTraits {
    BinaryTraits {
        glibc: None,
        macos_min: None,
        macho: false,
        universal: false,
        elf: true,
        dynamically_linked: false,
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
fn dynamic_gnu_target_without_glibc_requirement_errors() {
    // A *dynamically linked* gnu ELF with no glibc requirement is the wrong
    // (or verneed-stripped) binary — `traits()` models a healthy dynamic ELF.
    let err = platform_tag("x86_64-unknown-linux-gnu", &traits(None, None, false)).unwrap_err();
    assert!(err.to_string().contains("GLIBC"), "{err:#}");
}

#[test]
fn non_elf_under_gnu_target_errors() {
    // A non-ELF artifact routed under a gnu triple is the wrong binary: it is
    // neither dynamic-with-glibc nor a fully-static ELF, so it must hard-error
    // rather than ship an immutable wheel with a guessed tag.
    let not_elf = BinaryTraits {
        glibc: None,
        macos_min: None,
        macho: false,
        universal: false,
        elf: false,
        dynamically_linked: false,
    };
    let err = platform_tag("x86_64-unknown-linux-gnu", &not_elf).unwrap_err();
    assert!(err.to_string().contains("GLIBC"), "{err:#}");
}

#[test]
fn static_gnu_binary_tags_arch_aware_manylinux_floor() {
    // anodizer ships fully-static linux-gnu binaries: no PT_INTERP, no glibc
    // requirement. They run on any glibc host and must tag at the arch's
    // lowest manylinux floor, NOT hard-error.
    let s = static_gnu_traits();
    // x86_64/i686 → manylinux1 baseline (2_5).
    assert_eq!(
        platform_tag("x86_64-unknown-linux-gnu", &s).unwrap(),
        "manylinux_2_5_x86_64"
    );
    assert_eq!(
        platform_tag("i686-unknown-linux-gnu", &s).unwrap(),
        "manylinux_2_5_i686"
    );
    // aarch64's first manylinux profile is manylinux2014 (2_17); a
    // `manylinux_2_5_aarch64` tag names a platform pip never enumerates.
    assert_eq!(
        platform_tag("aarch64-unknown-linux-gnu", &s).unwrap(),
        "manylinux_2_17_aarch64"
    );
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
fn synthetic_darwin_universal_target_tags_universal2() {
    // The lipo'd fat binary carries the synthetic target `darwin-universal`,
    // which contains no `apple-darwin` substring — before the fix it fell
    // through every branch to the catch-all bail and stranded every pypi
    // backfill ("target 'darwin-universal' has no wheel platform-tag mapping").
    assert_eq!(
        platform_tag("darwin-universal", &traits(None, Some((11, 0)), true)).unwrap(),
        "macosx_11_0_universal2"
    );
    // `universal = false` on the traits must NOT matter: the synthetic target
    // is universal2 by construction (an archive `format: binary` clone selects
    // it as a plain UploadableBinary with `universal = false`).
    assert_eq!(
        platform_tag("darwin-universal", &traits(None, Some((11, 0)), false)).unwrap(),
        "macosx_11_0_universal2"
    );
    // No minos load command still tags at the arm64 floor.
    assert_eq!(
        platform_tag("darwin-universal", &traits(None, None, false)).unwrap(),
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
fn macosx_minor_clamps_to_zero_for_major_ge_11() {
    // C1: pip/packaging only enumerate `macosx_<major>_0` for macOS 11+, so a
    // real minos of 11.2 must tag `macosx_11_0`, not `macosx_11_2` (which is
    // uninstallable). maturin/cibuildwheel apply the same clamp.
    assert_eq!(
        platform_tag("aarch64-apple-darwin", &traits(None, Some((11, 2)), false)).unwrap(),
        "macosx_11_0_arm64"
    );
    assert_eq!(
        platform_tag("x86_64-apple-darwin", &traits(None, Some((12, 3)), false)).unwrap(),
        "macosx_12_0_x86_64"
    );
    // Pre-11 minors are preserved (Catalina 10.15 stays 10_15).
    assert_eq!(
        platform_tag("x86_64-apple-darwin", &traits(None, Some((10, 15)), false)).unwrap(),
        "macosx_10_15_x86_64"
    );
}

#[test]
fn manylinux_floors_below_baseline_glibc() {
    // C2: `GLIBC_2.2.5` (the ancient x86_64 baseline symbol) truncates to
    // (2, 2); `manylinux_2_2` is below every recognized manylinux platform,
    // so the tag floors to the manylinux1 baseline `manylinux_2_5`.
    assert_eq!(
        platform_tag(
            "x86_64-unknown-linux-gnu",
            &traits(Some((2, 2)), None, false)
        )
        .unwrap(),
        "manylinux_2_5_x86_64"
    );
    // A below-floor minor also lifts to 2_5.
    assert_eq!(
        platform_tag(
            "aarch64-unknown-linux-gnu",
            &traits(Some((2, 4)), None, false)
        )
        .unwrap(),
        "manylinux_2_5_aarch64"
    );
    // At/above the floor is preserved verbatim.
    assert_eq!(
        platform_tag(
            "x86_64-unknown-linux-gnu",
            &traits(Some((2, 5)), None, false)
        )
        .unwrap(),
        "manylinux_2_5_x86_64"
    );
    assert_eq!(
        platform_tag(
            "x86_64-unknown-linux-gnu",
            &traits(Some((2, 28)), None, false)
        )
        .unwrap(),
        "manylinux_2_28_x86_64"
    );
}

#[test]
fn darwin_non_macho_bytes_error() {
    // C3: a non-Mach-O artifact routed under a darwin triple is the wrong
    // binary and must hard-error (not silently ship a guessed fallback tag) —
    // the Mach-O analogue of the gnu "no GLIBC_*" error. A healthy Mach-O
    // missing its load command still falls back.
    let not_macho = BinaryTraits {
        glibc: None,
        macos_min: None,
        macho: false,
        universal: false,
        elf: false,
        dynamically_linked: false,
    };
    let err = platform_tag("x86_64-apple-darwin", &not_macho).unwrap_err();
    assert!(err.to_string().contains("not a Mach-O"), "{err:#}");

    // ELF bytes inspected under a darwin triple → macho:false → error.
    let inspected = inspect_binary(b"\x7fELF-not-a-macho", false).unwrap();
    assert!(!inspected.macho);
    assert!(platform_tag("aarch64-apple-darwin", &inspected).is_err());
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
        metadata_version: "2.1".to_string(),
        bin_name: "my-tool".to_string(),
        summary: Some("A tool".to_string()),
        description: Some("Long description".to_string()),
        description_content_type: None,
        author: None,
        author_email: None,
        license: Some("MIT".to_string()),
        homepage: Some("https://example.com".to_string()),
        project_urls: Vec::new(),
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
    // C6: Warehouse's deleted-file rejection burns the slot but does not
    // repeat "already exists" — skip_existing must fold it too.
    assert!(is_duplicate_rejection(
        400,
        "This filename has already been used, use a different version."
    ));
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
        tag_template: Some("v{{ .Version }}".to_string()),
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
        index_url: Some(repo),
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
            index_url: Some(repo),
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
            index_url: Some(format!("http://{addr}/legacy/")),
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

/// Add a binary carrying explicit metadata (e.g. `amd64_variant`), with
/// per-binary content so distinct artifacts hash differently.
fn add_binary_with_meta(
    ctx: &mut anodizer_core::context::Context,
    dir: &std::path::Path,
    target: &str,
    crate_name: &str,
    bin_name: &str,
    meta: &[(&str, &str)],
) {
    let path = dir.join(format!("{bin_name}-{target}"));
    std::fs::write(&path, format!("#!fake-{target}").as_bytes()).expect("write binary");
    let mut metadata = std::collections::HashMap::new();
    for (k, v) in meta {
        metadata.insert((*k).to_string(), (*v).to_string());
    }
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        path,
        name: bin_name.to_string(),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });
}

/// One scripted binary row: `(target, crate_name, bin_name, metadata pairs)`.
type BinaryRow<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// Run a publish against a scripted 200-index over the supplied crates and
/// binaries, mutating a base `PypiConfig` (with `index_url` prewired) before
/// the run. Owns the tempdir for the call's duration.
fn run_publish_with_binaries(
    crates: Vec<CrateConfig>,
    binaries: &[BinaryRow<'_>],
    cfg_for: impl FnOnce(String) -> PypiConfig,
) -> Vec<anodizer_core::publish_evidence::PypiFileSnapshot> {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let cfg = cfg_for(format!("http://{addr}/legacy/"));
    let mut ctx = publish_ctx(tmp.path(), crates, cfg);
    for (target, crate_name, bin_name, meta) in binaries {
        add_binary_with_meta(&mut ctx, tmp.path(), target, crate_name, bin_name, meta);
    }
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("publish");
    files
}

// -----------------------------------------------------------------------------
// METADATA gaps: Description-Content-Type, Project-URL map, Author headers
// -----------------------------------------------------------------------------

#[test]
fn metadata_emits_author_project_urls_and_content_type_before_body() {
    let mut s = spec("manylinux_2_28_x86_64");
    s.author = Some("Ada Lovelace".to_string());
    s.author_email = Some("ada@example.com".to_string());
    s.description_content_type = Some("text/markdown".to_string());
    s.project_urls = vec![
        (
            "Documentation".to_string(),
            "https://docs.example.com".to_string(),
        ),
        (
            "Repository".to_string(),
            "https://github.com/me/tool".to_string(),
        ),
    ];
    let md = render_metadata(&s);

    assert!(md.contains("Author: Ada Lovelace\n"), "{md}");
    assert!(md.contains("Author-email: ada@example.com\n"), "{md}");
    // The Homepage link plus both explicit project_urls, in supplied order.
    assert!(
        md.contains("Project-URL: Homepage, https://example.com\n"),
        "{md}"
    );
    assert!(
        md.contains("Project-URL: Documentation, https://docs.example.com\n"),
        "{md}"
    );
    assert!(
        md.contains("Project-URL: Repository, https://github.com/me/tool\n"),
        "{md}"
    );
    // Content-Type header must precede the blank line + body (pip reads it to
    // pick the renderer).
    let ct = md
        .find("Description-Content-Type: text/markdown\n")
        .expect("ct header");
    let body = md.find("\nLong description\n").expect("body");
    assert!(
        ct < body,
        "content-type header must precede the body:\n{md}"
    );
}

#[test]
fn metadata_omits_new_headers_when_unset() {
    // The default `spec()` sets none of the new fields → no stray headers.
    let md = render_metadata(&spec("musllinux_1_2_x86_64"));
    assert!(!md.contains("Author:"), "{md}");
    assert!(!md.contains("Author-email:"), "{md}");
    assert!(!md.contains("Description-Content-Type:"), "{md}");
    // Only the Homepage-derived Project-URL, no extras.
    assert_eq!(md.matches("Project-URL:").count(), 1, "{md}");
}

#[test]
fn content_type_defaults_to_markdown_when_description_present() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .dist(tmp.path().join("dist"))
        .build();

    // Description present, content-type unset → defaults to text/markdown so
    // PyPI renders Markdown instead of raw plaintext.
    let with_desc = PypiConfig {
        description: Some("# Heading".into()),
        ..Default::default()
    };
    let spec = build_spec_base(&ctx, &with_desc, "demo", "1.2.3", "demo").expect("spec");
    assert_eq!(spec.description.as_deref(), Some("# Heading"));
    assert_eq!(
        spec.description_content_type.as_deref(),
        Some("text/markdown")
    );

    // Explicit override wins.
    let rst = PypiConfig {
        description: Some("body".into()),
        description_content_type: Some("text/x-rst".into()),
        ..Default::default()
    };
    let spec = build_spec_base(&ctx, &rst, "demo", "1.2.3", "demo").expect("spec");
    assert_eq!(spec.description_content_type.as_deref(), Some("text/x-rst"));

    // No description body at all → no content-type header.
    let bare = PypiConfig::default();
    let spec = build_spec_base(&ctx, &bare, "demo", "1.2.3", "demo").expect("spec");
    assert!(spec.description.is_none());
    assert!(spec.description_content_type.is_none());
}

#[test]
fn author_and_project_urls_render_through_templates() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .dist(tmp.path().join("dist"))
        .build();
    let mut project_urls = std::collections::BTreeMap::new();
    project_urls.insert(
        "Version".to_string(),
        "https://ex/{{ .Version }}".to_string(),
    );
    project_urls.insert("Repo".to_string(), "https://ex/repo".to_string());
    let cfg = PypiConfig {
        author: Some("Grace {{ .ProjectName }}".into()),
        author_email: Some("g@example.com".into()),
        project_urls: Some(project_urls),
        ..Default::default()
    };
    let spec = build_spec_base(&ctx, &cfg, "demo", "1.2.3", "demo").expect("spec");
    assert_eq!(spec.author.as_deref(), Some("Grace demo"));
    assert_eq!(spec.author_email.as_deref(), Some("g@example.com"));
    // BTreeMap order → Repo before Version; the URL template is rendered.
    assert_eq!(
        spec.project_urls,
        vec![
            ("Repo".to_string(), "https://ex/repo".to_string()),
            ("Version".to_string(), "https://ex/1.2.3".to_string()),
        ]
    );
}

// -----------------------------------------------------------------------------
// P4: per-target platform-tag overrides (all three config modes)
// -----------------------------------------------------------------------------

fn override_cfg(repo: String, extra: impl FnOnce(&mut PypiConfig)) -> PypiConfig {
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert(
        "aarch64-unknown-linux-gnu".to_string(),
        "manylinux_2_28_aarch64".to_string(),
    );
    let mut cfg = PypiConfig {
        index_url: Some(repo),
        platform_tag_overrides: Some(overrides),
        ..Default::default()
    };
    extra(&mut cfg);
    cfg
}

#[test]
fn platform_tag_override_wins_single_crate() {
    // The fake bytes carry no GLIBC symbol, so the auto path would BAIL for a
    // gnu target — the override proves inspection is skipped entirely and the
    // configured tag is used verbatim (the git-cliff manylinux_2_28 case).
    let files = run_publish_with_binaries(
        vec![demo_crate("demo", ".")],
        &[("aarch64-unknown-linux-gnu", "demo", "demo", &[])],
        |repo| override_cfg(repo, |_| {}),
    );
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].filename,
        "demo-1.2.3-py3-none-manylinux_2_28_aarch64.whl"
    );
    assert_eq!(files[0].platform_tag, "manylinux_2_28_aarch64");
}

#[test]
fn platform_tag_override_wins_lockstep_workspace() {
    let files = run_publish_with_binaries(
        vec![demo_crate("demo", "."), demo_crate("other", "other")],
        &[("aarch64-unknown-linux-gnu", "demo", "demo", &[])],
        |repo| {
            override_cfg(repo, |c| {
                c.name = Some("demo".into());
                c.ids = Some(vec!["demo".into()]);
            })
        },
    );
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].platform_tag, "manylinux_2_28_aarch64");
}

#[test]
fn platform_tag_override_wins_per_crate() {
    // Per-crate topology: two crates, entry scoped by ids to `other`.
    let files = run_publish_with_binaries(
        vec![demo_crate("demo", "."), demo_crate("other", "other")],
        &[
            ("x86_64-unknown-linux-musl", "demo", "demo", &[]),
            ("aarch64-unknown-linux-gnu", "other", "other", &[]),
        ],
        |repo| {
            override_cfg(repo, |c| {
                c.ids = Some(vec!["other".into()]);
            })
        },
    );
    assert_eq!(files.len(), 1, "only the scoped crate's binary ships");
    assert!(files[0].filename.starts_with("other-1.2.3"));
    assert_eq!(files[0].platform_tag, "manylinux_2_28_aarch64");
}

#[test]
fn no_override_keeps_auto_detected_tag() {
    // Same aarch64-gnu triple but no override for it → the auto path runs; a
    // musl binary (no glibc needed) tags musllinux from inspection, unchanged.
    let files = run_publish_with_binaries(
        vec![demo_crate("demo", ".")],
        &[("aarch64-unknown-linux-musl", "demo", "demo", &[])],
        |repo| PypiConfig {
            index_url: Some(repo),
            ..Default::default()
        },
    );
    assert_eq!(files[0].platform_tag, "musllinux_1_2_aarch64");
}

// -----------------------------------------------------------------------------
// F7: microarch variant selection
// -----------------------------------------------------------------------------

#[test]
fn amd64_variant_selects_matching_microarch_build() {
    // Two amd64 binaries on the SAME triple, tagged v1 (baseline) and v3.
    // Selecting v3 must drop the v1 build — if both survived they would derive
    // the same platform tag and the publish would bail on the collision, so a
    // clean single-file publish is the proof the filter chose one.
    let files = run_publish_with_binaries(
        vec![demo_crate("demo", ".")],
        &[
            (
                "x86_64-unknown-linux-musl",
                "demo",
                "demo",
                &[("amd64_variant", "v1")],
            ),
            (
                "x86_64-unknown-linux-musl",
                "demo",
                "demo",
                &[("amd64_variant", "v3")],
            ),
        ],
        |repo| PypiConfig {
            index_url: Some(repo),
            amd64_variant: Some(anodizer_core::config::Amd64Variant::V3),
            ..Default::default()
        },
    );
    assert_eq!(
        files.len(),
        1,
        "only the v3 microarch build becomes the wheel"
    );
    assert_eq!(files[0].platform_tag, "musllinux_1_2_x86_64");
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
        index_url: Some(format!("http://{addr}/legacy/")),
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
    // The legacy `repository:` spelling deserializes into `index_url` via
    // serde alias.
    assert_eq!(
        e.index_url.as_deref(),
        Some("https://test.pypi.org/legacy/")
    );
}

// -----------------------------------------------------------------------------
// Publisher contract
// -----------------------------------------------------------------------------

#[test]
fn pypi_publisher_classification() {
    let p = PypiPublisher::new();
    assert_eq!(p.name(), "pypi");
    assert_eq!(p.group(), anodizer_core::PublisherGroup::Submitter);
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
    // Default auth is `auto`: the token ladder rides inside a coarse
    // token-OR-oidc any-of, so assert containment rather than exact equality.
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::EnvAnyOf { vars }
                if vars.contains(&"PYPI_TOKEN".to_string())
                    && vars.contains(&"MATURIN_PYPI_TOKEN".to_string())
        )),
        "token ladder is part of the auto any-of: {reqs:?}"
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
// Trusted Publishing (OIDC)
// -----------------------------------------------------------------------------

#[test]
fn mint_token_url_maps_only_pypi_hosts() {
    use super::oidc::mint_token_url;
    assert_eq!(
        mint_token_url("https://upload.pypi.org/legacy/").as_deref(),
        Some("https://pypi.org/_/oidc/mint-token")
    );
    assert_eq!(
        mint_token_url("https://test.pypi.org/legacy/").as_deref(),
        Some("https://test.pypi.org/_/oidc/mint-token")
    );
    // A custom index has no Trusted-Publishing contract.
    assert_eq!(mint_token_url("https://pypi.example.com/legacy/"), None);
}

#[test]
fn oidc_context_available_requires_both_request_vars() {
    let base = || {
        TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.2.3")
            .crates(vec![demo_crate("demo", ".")])
    };
    // Neither var → no context.
    let ctx = base().build();
    assert!(!super::oidc::oidc_context_available(&ctx));
    // Only one var → still no context.
    let ctx = base()
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "https://actions/x")
        .build();
    assert!(!super::oidc::oidc_context_available(&ctx));
    // Both non-empty → context present.
    let mut ctx = base().build();
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ACTIONS_ID_TOKEN_REQUEST_URL", "https://actions/x")
            .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "req-tok"),
    );
    assert!(super::oidc::oidc_context_available(&ctx));
}

#[test]
fn requirements_gate_on_auth_mode() {
    use anodizer_core::config::PypiAuthMode;
    let ctx_with = |auth: PypiAuthMode| {
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.2.3")
            .crates(vec![demo_crate("demo", ".")])
            .build();
        ctx.config.pypis = Some(vec![PypiConfig {
            auth,
            ..Default::default()
        }]);
        ctx
    };
    let oidc_all = anodizer_core::EnvRequirement::EnvAllOf {
        vars: vec![
            "ACTIONS_ID_TOKEN_REQUEST_URL".to_string(),
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN".to_string(),
        ],
    };

    // Token: the token ladder any-of, no OIDC requirement.
    let reqs = PypiPublisher::new().requirements(&ctx_with(PypiAuthMode::Token));
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::EnvAnyOf { vars }
                if vars == &vec!["PYPI_TOKEN".to_string(), "MATURIN_PYPI_TOKEN".to_string()]
        )),
        "token mode requires the token ladder: {reqs:?}"
    );
    assert!(!reqs.contains(&oidc_all), "token mode must not demand OIDC");

    // Oidc: strictly the Actions request pair; the token ladder is NOT required.
    let reqs = PypiPublisher::new().requirements(&ctx_with(PypiAuthMode::Oidc));
    assert!(
        reqs.contains(&oidc_all),
        "oidc mode demands the pair: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::EnvAnyOf { vars }
                if vars == &vec!["PYPI_TOKEN".to_string(), "MATURIN_PYPI_TOKEN".to_string()]
        )),
        "oidc mode must not require a token: {reqs:?}"
    );

    // Auto: a coarse any-of accepting either a token var OR an OIDC context.
    let reqs = PypiPublisher::new().requirements(&ctx_with(PypiAuthMode::Auto));
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::EnvAnyOf { vars }
                if vars.contains(&"PYPI_TOKEN".to_string())
                    && vars.contains(&"ACTIONS_ID_TOKEN_REQUEST_URL".to_string())
        )),
        "auto mode is token-OR-oidc: {reqs:?}"
    );
}

#[test]
fn oidc_mint_errors_without_request_env() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    let err = super::oidc::mint_trusted_publishing_token(
        &ctx,
        "https://upload.pypi.org/legacy/",
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        &ctx.logger("publish"),
    )
    .expect_err("missing OIDC request env must error");
    assert!(
        err.to_string().contains("ACTIONS_ID_TOKEN_REQUEST_URL"),
        "{err}"
    );
}

#[test]
fn oidc_mode_ignores_a_malformed_inline_token() {
    use super::publisher::resolve_upload_credential;
    // The token field is "Unused when auth: oidc": a stray/malformed inline
    // token template must NOT be resolved (and abort) before the mint path.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    let cfg = PypiConfig {
        auth: anodizer_core::config::PypiAuthMode::Oidc,
        token: Some("{{ this_is_not_a_real_filter }}".into()),
        ..Default::default()
    };
    // No OIDC request env → must fail on the OIDC path (missing
    // ACTIONS_ID_TOKEN_REQUEST_URL), NOT on rendering the unused token.
    let err = resolve_upload_credential(
        &ctx,
        &cfg,
        "https://upload.pypi.org/legacy/",
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        "pypis[0]",
        &ctx.logger("publish"),
    )
    .expect_err("oidc without request env must error");
    let msg = err.to_string();
    assert!(
        msg.contains("ACTIONS_ID_TOKEN_REQUEST_URL"),
        "must fail on the OIDC path, not the unused token template: {msg}"
    );
    assert!(
        !msg.contains("render token template"),
        "the unused token must never be rendered: {msg}"
    );
}

#[test]
fn auto_mode_routes_around_a_token_render_error_to_oidc() {
    use super::publisher::resolve_upload_credential;
    // auto's contract is "use whatever credential the environment offers": a
    // `token:` template that fails to render must NOT abort the run when an
    // OIDC context is present — it routes to Trusted Publishing instead. With
    // a full OIDC context the mint path is taken, so the error is an OIDC-path
    // error, never "render token template".
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ACTIONS_ID_TOKEN_REQUEST_URL", "https://actions/x")
            .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "req-tok"),
    );
    let cfg = PypiConfig {
        auth: anodizer_core::config::PypiAuthMode::Auto,
        token: Some("{{ this_is_not_a_real_filter }}".into()),
        ..Default::default()
    };
    let err = resolve_upload_credential(
        &ctx,
        &cfg,
        // Custom index → the mint path fast-fails deterministically offline.
        "https://pypi.example.com/legacy/",
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        "pypis[0]",
        &ctx.logger("publish"),
    )
    .expect_err("mint on a custom index must error");
    let msg = err.to_string();
    assert!(
        msg.contains("only supported against"),
        "auto must route the token error to the OIDC mint path: {msg}"
    );
    assert!(
        !msg.contains("render token template"),
        "a token-render error must not abort auto when OIDC is available: {msg}"
    );
}

#[test]
fn auto_mode_surfaces_the_token_render_error_without_oidc() {
    use super::publisher::resolve_upload_credential;
    // No OIDC fallback to route around to: the token-render error IS the
    // actionable failure and must be surfaced (not swallowed into the generic
    // no-credential guidance).
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    let cfg = PypiConfig {
        auth: anodizer_core::config::PypiAuthMode::Auto,
        token: Some("{{ this_is_not_a_real_filter }}".into()),
        ..Default::default()
    };
    let err = resolve_upload_credential(
        &ctx,
        &cfg,
        "https://upload.pypi.org/legacy/",
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        "pypis[0]",
        &ctx.logger("publish"),
    )
    .expect_err("a malformed token template with no OIDC fallback must error");
    assert!(
        err.to_string().contains("render token template"),
        "the token-render error must surface when there is no OIDC fallback: {err}"
    );
}

#[test]
fn oidc_mint_errors_on_custom_index() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("demo", ".")])
        .build();
    // Even with a full OIDC context, a custom index has no mint endpoint.
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ACTIONS_ID_TOKEN_REQUEST_URL", "https://actions/x")
            .with("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "req-tok"),
    );
    let err = super::oidc::mint_trusted_publishing_token(
        &ctx,
        "https://pypi.example.com/legacy/",
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        &ctx.logger("publish"),
    )
    .expect_err("custom index must error under oidc");
    assert!(err.to_string().contains("only supported against"), "{err}");
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
        index_url: Some("not a url".into()),
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

// -----------------------------------------------------------------------------
// C8 — anchored simple-index probe
// -----------------------------------------------------------------------------

#[test]
fn simple_index_probe_matches_exact_version_only() {
    use super::publisher::body_lists_version;
    let page = r#"<!DOCTYPE html><html><body>
        <a href="/x/foo-1.2.30-py3-none-any.whl">foo-1.2.30-py3-none-any.whl</a>
        <a href="/x/foo-1.2.3rc1-py3-none-any.whl">foo-1.2.3rc1-py3-none-any.whl</a>
        <a href="/x/foo-1.2.3.post1.tar.gz">foo-1.2.3.post1.tar.gz</a>
        </body></html>"#;
    // A 1.2.3 probe must NOT fire on 1.2.30 / 1.2.3rc1 / 1.2.3.post1.
    assert!(!body_lists_version(page, "foo", "1.2.3"));

    // The exact version present as a wheel → match.
    let with_wheel = r#"<a href="/x/foo-1.2.3-py3-none-manylinux_2_28_x86_64.whl">w</a>"#;
    assert!(body_lists_version(with_wheel, "foo", "1.2.3"));

    // The exact version present as an sdist → match.
    let with_sdist = r#"<a href="/x/foo-1.2.3.tar.gz">s</a>"#;
    assert!(body_lists_version(with_sdist, "foo", "1.2.3"));

    // Name is compared PEP 503-normalized (foo_bar wheel vs foo-bar probe).
    let underscore = r#"<a>foo_bar-1.2.3-py3-none-any.whl</a>"#;
    assert!(body_lists_version(underscore, "foo-bar", "1.2.3"));
    // A different distribution at the same version does not match.
    assert!(!body_lists_version(underscore, "foo", "1.2.3"));
}

// -----------------------------------------------------------------------------
// C7 — sdist upload echoes PKG-INFO's own metadata_version + version
// -----------------------------------------------------------------------------

/// Build a minimal sdist `.tar.gz` carrying `<name>-<version>/PKG-INFO` with
/// the given headers.
fn write_fake_sdist(
    dir: &std::path::Path,
    name: &str,
    version: &str,
    pkg_info: &str,
) -> std::path::PathBuf {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let path = dir.join(format!("{name}-{version}.tar.gz"));
    let file = std::fs::File::create(&path).expect("create sdist");
    let enc = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(enc);
    let entry_path = format!("{name}-{version}/PKG-INFO");
    let bytes = pkg_info.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path(&entry_path).expect("set path");
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, bytes).expect("append PKG-INFO");
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gz");
    path
}

#[test]
fn parse_pkg_info_reads_maturin_metadata_and_version() {
    use super::sdist::{SdistPkgInfo, parse_pkg_info};
    let tmp = tempfile::TempDir::new().expect("tmp");
    // maturin emits its own Metadata-Version (2.4) and the pyproject version,
    // neither of which matches anodizer's wheel METADATA (2.1) or a cargo
    // version — the upload form must carry THESE.
    let pkg_info = "Metadata-Version: 2.4\nName: my-tool\nVersion: 9.9.9\nSummary: x\n\nbody\n";
    let path = write_fake_sdist(tmp.path(), "my_tool", "9.9.9", pkg_info);
    assert_eq!(
        parse_pkg_info(&path).expect("parse"),
        SdistPkgInfo {
            metadata_version: "2.4".to_string(),
            name: "my-tool".to_string(),
            version: "9.9.9".to_string(),
        }
    );
}

#[test]
fn parse_pkg_info_errors_when_no_top_level_pkg_info() {
    // A sdist whose only PKG-INFO lives deeper than the top level (e.g. the
    // `*.egg-info/PKG-INFO`) must be rejected — the upload form has no
    // metadata to echo.
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let tmp = tempfile::TempDir::new().expect("tmp");
    let path = tmp.path().join("nometa-1.0.tar.gz");
    let enc = GzEncoder::new(
        std::fs::File::create(&path).unwrap(),
        Compression::default(),
    );
    let mut builder = tar::Builder::new(enc);
    for entry in ["nometa-1.0/README", "nometa-1.0/nometa.egg-info/PKG-INFO"] {
        let body = b"noise";
        let mut header = tar::Header::new_gnu();
        header.set_path(entry).unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &body[..]).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();

    let err = super::sdist::parse_pkg_info(&path).expect_err("must reject missing PKG-INFO");
    assert!(
        err.to_string().contains("no top-level PKG-INFO"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn parse_pkg_info_errors_when_required_header_missing() {
    // PKG-INFO present but missing the `Version` header → the three-field
    // guard bails.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let pkg_info = "Metadata-Version: 2.4\nName: my-tool\nSummary: x\n\nbody\n";
    let path = write_fake_sdist(tmp.path(), "my_tool", "1.0", pkg_info);
    let err = super::sdist::parse_pkg_info(&path).expect_err("missing Version must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing a required header") && msg.contains("Version"),
        "unexpected error: {msg}"
    );
}

#[test]
fn parse_pkg_info_tolerates_colonless_lines() {
    // A stray header line without a colon is skipped, not fatal; the real
    // headers after it still parse.
    use super::sdist::{SdistPkgInfo, parse_pkg_info};
    let tmp = tempfile::TempDir::new().expect("tmp");
    let pkg_info =
        "Metadata-Version: 2.4\nGARBAGE-NO-COLON\nName: my-tool\nVersion: 3.1.4\n\nbody\n";
    let path = write_fake_sdist(tmp.path(), "my_tool", "3.1.4", pkg_info);
    assert_eq!(
        parse_pkg_info(&path).expect("parse"),
        SdistPkgInfo {
            metadata_version: "2.4".to_string(),
            name: "my-tool".to_string(),
            version: "3.1.4".to_string(),
        }
    );
}

#[test]
fn build_sdist_bails_when_manifest_missing() {
    // The early guard fires before maturin is ever spawned, so an absent
    // pyproject.toml is a clean, actionable error rather than a subprocess
    // failure.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let ctx = TestContextBuilder::new().build();
    let out = tmp.path().join("staging");
    let err = super::sdist::build_sdist(&ctx, tmp.path().to_str().unwrap(), &out, &probe_logger())
        .expect_err("missing pyproject.toml must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not exist") && msg.contains("sdist_manifest"),
        "unexpected error: {msg}"
    );
}

#[test]
fn sdist_upload_form_carries_pkg_info_metadata_version() {
    // The upload form's metadata_version comes from the spec, so an sdist spec
    // stamped from PKG-INFO (2.4) sends 2.4, not the hardcoded wheel 2.1.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut s = spec("source");
    s.metadata_version = "2.4".to_string();
    s.version = "9.9.9".to_string();
    let file = tmp.path().join("my_tool-9.9.9.tar.gz");
    std::fs::write(&file, b"fake-sdist-bytes").expect("write");
    let client =
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(5)).expect("client");
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: Some(1),
    }]);
    upload_file(
        &client,
        &format!("http://{addr}/legacy/"),
        "tok",
        "my-tool",
        &s,
        FileType::Sdist,
        &file,
        true,
        &anodizer_core::retry::RetryPolicy::PREFLIGHT,
        None,
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("upload");
    let entries = log.lock().unwrap();
    let body = &entries[0].body;
    assert!(
        body.contains("name=\"metadata_version\"\r\n\r\n2.4"),
        "form must carry PKG-INFO metadata_version 2.4: {body}"
    );
    assert!(
        body.contains("name=\"version\"\r\n\r\n9.9.9"),
        "form must carry PKG-INFO version: {body}"
    );
    assert!(body.contains("name=\"filetype\"\r\n\r\nsdist"), "{body}");
}

// -----------------------------------------------------------------------------
// C15 — per-entry metadata scoped to the entry's crate (ids)
// -----------------------------------------------------------------------------

#[test]
fn metadata_scopes_to_entry_crate_via_ids() {
    use anodizer_core::config::MetadataConfig;
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    // Per-crate workspace: two crates with DIFFERENT metadata. An entry scoped
    // to `other` must publish under other's name/license/homepage, never the
    // primary crate's.
    let mut ctx = publish_ctx(
        tmp.path(),
        vec![demo_crate("demo", "."), demo_crate("other", "other")],
        PypiConfig {
            ids: Some(vec!["other".into()]),
            index_url: Some(format!("http://{addr}/legacy/")),
            ..Default::default()
        },
    );
    ctx.config.derived_metadata.insert(
        "demo".into(),
        MetadataConfig {
            license: Some("MIT".into()),
            homepage: Some("https://demo.example".into()),
            description: Some("demo summary".into()),
            ..Default::default()
        },
    );
    ctx.config.derived_metadata.insert(
        "other".into(),
        MetadataConfig {
            license: Some("Apache-2.0".into()),
            homepage: Some("https://other.example".into()),
            description: Some("other summary".into()),
            ..Default::default()
        },
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-unknown-linux-musl",
        "other",
        "other",
    );
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("publish");
    assert_eq!(files.len(), 1);
    assert!(
        files[0].filename.starts_with("other-1.2.3"),
        "{:?}",
        files[0]
    );

    // Read the staged wheel's METADATA and assert other's identity, not demo's.
    let staging = tmp.path().join("dist").join("pypi").join("pypis[0]");
    let wheel = std::fs::read_dir(&staging)
        .expect("staging dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "whl"))
        .expect("a wheel");
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&wheel).expect("open")).expect("zip");
    let meta = read_entry(&mut zip, "other-1.2.3.dist-info/METADATA");
    assert!(meta.contains("Name: other\n"), "{meta}");
    assert!(meta.contains("License: Apache-2.0\n"), "{meta}");
    assert!(
        meta.contains("Project-URL: Homepage, https://other.example\n"),
        "{meta}"
    );
    assert!(meta.contains("Summary: other summary\n"), "{meta}");
    assert!(
        !meta.contains("MIT"),
        "demo's license must not leak: {meta}"
    );
}

// -----------------------------------------------------------------------------
// C19 — metadata template errors propagate (never ship raw source)
// -----------------------------------------------------------------------------

#[test]
fn metadata_template_error_aborts_publish() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = publish_ctx(
        tmp.path(),
        vec![demo_crate("demo", ".")],
        PypiConfig {
            // Unterminated template — must error, not ship "{{ " raw into
            // immutable METADATA.
            summary: Some("{{ ".into()),
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
    assert!(err.to_string().contains("summary"), "{err:#}");
    assert!(files.is_empty());
}

// -----------------------------------------------------------------------------
// C11 — config-time platform-tag collision preflight
// -----------------------------------------------------------------------------

fn crate_with_targets(name: &str, path: &str, targets: &[&str]) -> CrateConfig {
    use anodizer_core::config::BuildConfig;
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        builds: Some(vec![BuildConfig {
            binary: Some(name.to_string()),
            targets: Some(targets.iter().map(|t| t.to_string()).collect()),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

#[test]
fn preflight_warns_on_cross_crate_platform_tag_collision() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![
            crate_with_targets("demo", ".", &["x86_64-unknown-linux-gnu"]),
            crate_with_targets("other", "other", &["x86_64-unknown-linux-gnu"]),
        ])
        .build();
    // No `ids:` → both crates selected; both build the same triple → same
    // wheel platform tag → identical filename collision. `not a url` keeps the
    // version probe offline.
    ctx.config.pypis = Some(vec![PypiConfig {
        index_url: Some("not a url".into()),
        ..Default::default()
    }]);
    match PypiPublisher::new().preflight(&ctx).expect("preflight") {
        PreflightCheck::Warning(m) => {
            assert!(m.contains("same target triple"), "{m}");
            assert!(m.contains("x86_64-unknown-linux-gnu"), "{m}");
        }
        other => panic!("expected Warning, got {other:?}"),
    }
}

#[test]
fn preflight_passes_single_crate_multi_target() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![crate_with_targets(
            "demo",
            ".",
            &["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"],
        )])
        .build();
    // Distinct triples in one crate never collide (different platform tags).
    ctx.config.pypis = Some(vec![PypiConfig {
        index_url: Some("not a url".into()),
        ..Default::default()
    }]);
    assert!(matches!(
        PypiPublisher::new().preflight(&ctx).expect("preflight"),
        PreflightCheck::Pass
    ));
}

// -----------------------------------------------------------------------------
// Rollback burn-probe surface: static (context-free) name/repository
// resolvers + the fail-closed live-index probe.
// -----------------------------------------------------------------------------

#[test]
fn static_project_name_prefers_cfg_name_else_crate() {
    use super::publisher::static_project_name;
    let named = PypiConfig {
        name: Some("my-tool".into()),
        ..Default::default()
    };
    assert_eq!(
        static_project_name("mycrate", &named),
        Some("my-tool".to_string())
    );
    // Unset name falls back to the crate name.
    assert_eq!(
        static_project_name("mycrate", &PypiConfig::default()),
        Some("mycrate".to_string())
    );
    // A templated name is unresolvable outside a release run — fail closed.
    let templated = PypiConfig {
        name: Some("{{ .ProjectName }}".into()),
        ..Default::default()
    };
    assert_eq!(static_project_name("mycrate", &templated), None);
}

#[test]
fn static_repository_defaults_and_rejects_template() {
    use super::publisher::static_repository;
    // Unset → production PyPI default.
    let def = static_repository(&PypiConfig::default()).expect("default repo");
    assert!(def.contains("pypi.org"), "default is public PyPI: {def}");
    // Explicit static value is preserved.
    assert_eq!(
        static_repository(&PypiConfig {
            index_url: Some("https://test.pypi.org/legacy/".into()),
            ..Default::default()
        }),
        Some("https://test.pypi.org/legacy/".to_string())
    );
    // Templated repository → unresolvable host → fail closed.
    assert_eq!(
        static_repository(&PypiConfig {
            index_url: Some("https://{{ .Env.HOST }}/legacy/".into()),
            ..Default::default()
        }),
        None
    );
}

#[test]
fn static_entry_crate_name_prefers_ids_then_primary() {
    use super::publisher::static_entry_crate_name;
    let mut config = Config {
        project_name: "proj".into(),
        ..Default::default()
    };
    config.crates = vec![CrateConfig {
        name: "primary".into(),
        ..Default::default()
    }];
    // `ids` first non-empty wins.
    assert_eq!(
        static_entry_crate_name(
            &config,
            &PypiConfig {
                ids: Some(vec!["chosen".into()]),
                ..Default::default()
            }
        ),
        "chosen"
    );
    // Else the primary crate.
    assert_eq!(
        static_entry_crate_name(&config, &PypiConfig::default()),
        "primary"
    );
    // Else the project name (no crates).
    config.crates.clear();
    assert_eq!(
        static_entry_crate_name(&config, &PypiConfig::default()),
        "proj"
    );
}

fn probe_policy() -> anodizer_core::retry::RetryPolicy {
    anodizer_core::retry::RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    }
}

fn probe_logger() -> anodizer_core::log::StageLogger {
    anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet)
}

#[test]
fn pypi_version_live_reports_true_on_simple_index_hit() {
    use super::publisher::pypi_version_live;
    // A custom-host repository routes through the PEP 503 /simple/ page; a
    // released wheel of the exact version means the slot is burned.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/simple/my-tool/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 61\r\n\r\n\
                   <a href=\"/x/my_tool-1.2.3-py3-none-any.whl\">my_tool-1.2.3</a>",
        times: None,
    }]);
    assert!(
        pypi_version_live(
            &format!("http://{addr}/legacy/"),
            "my-tool",
            "1.2.3",
            &probe_policy(),
            &probe_logger(),
        )
        .expect("reachable index resolves")
    );
}

#[test]
fn pypi_version_live_normalizes_prerelease_to_pep440() {
    use super::publisher::pypi_version_live;
    // The publisher uploads a pre-release under its PEP 440 form
    // (`v1.2.3-rc.1` → `1.2.3rc1`), so the rollback probe must query the SAME
    // string. The index lists only the PEP 440 filename; a probe passing the
    // raw semver `1.2.3-rc.1` would miss it and read the burned slot as free.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/simple/my-tool/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 67\r\n\r\n\
                   <a href=\"/x/my_tool-1.2.3rc1-py3-none-any.whl\">my_tool-1.2.3rc1</a>",
        times: None,
    }]);
    assert!(
        pypi_version_live(
            &format!("http://{addr}/legacy/"),
            "my-tool",
            "1.2.3-rc.1",
            &probe_policy(),
            &probe_logger(),
        )
        .expect("reachable index resolves")
    );
}

#[test]
fn pypi_version_live_reports_false_when_absent_from_simple_index() {
    use super::publisher::pypi_version_live;
    // Page exists but lists only a different version → positively absent.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/simple/my-tool/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 61\r\n\r\n\
                   <a href=\"/x/my_tool-9.9.9-py3-none-any.whl\">my_tool-9.9.9</a>",
        times: None,
    }]);
    assert!(
        !pypi_version_live(
            &format!("http://{addr}/legacy/"),
            "my-tool",
            "1.2.3",
            &probe_policy(),
            &probe_logger(),
        )
        .expect("reachable index resolves")
    );
}

#[test]
fn pypi_version_live_fails_closed_when_unreachable() {
    use super::publisher::pypi_version_live;
    // Nothing listens on 127.0.0.1:1 → the /simple/ GET fails at the
    // transport layer. An unreachable index must surface Err so the rollback
    // guard fails closed (never mistaking an outage for "not published").
    let err = pypi_version_live(
        "http://127.0.0.1:1/legacy/",
        "my-tool",
        "1.2.3",
        &probe_policy(),
        &probe_logger(),
    );
    assert!(
        err.is_err(),
        "an unreachable index must surface Err, got {err:?}"
    );
}

// -----------------------------------------------------------------------------
// targets: allowlist (subset of built targets; win_amd64 collision avoidance)
// -----------------------------------------------------------------------------

/// Without a `targets:` allowlist, a build that ships BOTH
/// `x86_64-pc-windows-msvc` and `x86_64-pc-windows-gnu` derives the SAME
/// `win_amd64` platform tag twice and collides on one `.whl` — the run-path
/// hard gate. This is the shape `targets:` exists to tame.
#[test]
fn targets_allowlist_none_hits_win_amd64_collision() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let cfg = PypiConfig {
        // A bogus repository is never contacted: the collision bail fires
        // inside the wheel loop, before any upload.
        index_url: Some("http://127.0.0.1:1/legacy/".into()),
        ..Default::default()
    };
    let mut ctx = publish_ctx(tmp.path(), vec![demo_crate("demo", ".")], cfg);
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-pc-windows-msvc",
        "demo",
        "demo",
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-pc-windows-gnu",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    let err = publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files)
        .expect_err("colliding win_amd64 wheels must hard-error");
    assert!(
        format!("{err:#}").contains("same wheel platform tag"),
        "error must name the tag collision: {err:#}"
    );
}

/// With `targets:` listing only the msvc triple, the colliding
/// `x86_64-pc-windows-gnu` binary is silently dropped, so exactly one
/// `win_amd64` wheel is built and uploaded — the collision is gone.
#[test]
fn targets_allowlist_prevents_win_amd64_collision() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/legacy/",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let cfg = PypiConfig {
        index_url: Some(format!("http://{addr}/legacy/")),
        targets: Some(vec!["x86_64-pc-windows-msvc".into()]),
        ..Default::default()
    };
    let mut ctx = publish_ctx(tmp.path(), vec![demo_crate("demo", ".")], cfg);
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-pc-windows-msvc",
        "demo",
        "demo",
    );
    add_binary(
        &mut ctx,
        tmp.path(),
        "x86_64-pc-windows-gnu",
        "demo",
        "demo",
    );
    let mut files = Vec::new();
    publish_to_pypi(&ctx, &ctx.logger("publish"), &mut files).expect("publish");
    assert_eq!(files.len(), 1, "only the msvc wheel survives the allowlist");
    assert_eq!(files[0].platform_tag, "win_amd64");
    assert_eq!(files[0].filename, "demo-1.2.3-py3-none-win_amd64.whl");
    assert_eq!(log.lock().unwrap().len(), 1, "exactly one upload");
}

/// Config-time validation: a `targets:` triple no selected build produces is a
/// Blocker naming the offending triple, labelled `pypi`.
#[test]
fn targets_allowlist_unbuilt_triple_blocks() {
    use anodizer_core::config::BuildConfig;
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            builds: Some(vec![BuildConfig {
                binary: Some("demo".into()),
                targets: Some(vec!["x86_64-unknown-linux-gnu".into()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();
    let targets = vec![
        "x86_64-unknown-linux-gnu".to_string(),
        "x86_64-foo-bar".to_string(),
    ];
    match crate::publisher_helpers::targets_allowlist_check(&ctx, Some(&targets), None, "pypi") {
        PreflightCheck::Blocker(m) => {
            assert!(m.contains("x86_64-foo-bar"), "names the triple: {m}");
            assert!(m.contains("pypi"), "labels the publisher: {m}");
            assert!(
                !m.contains("x86_64-unknown-linux-gnu"),
                "the built triple is not flagged: {m}"
            );
        }
        other => panic!("expected Blocker, got {other:?}"),
    }
}

/// A crate with no explicit `builds:` block but a real `src/main.rs` gets a
/// synthesized default build over `defaults.targets`, so a `targets:` allowlist
/// naming one of those triples must Pass. Guards against re-deriving the
/// universe from `c.builds` (which is `None` here and would false-block).
#[test]
fn targets_allowlist_synthesized_default_build_passes() {
    use anodizer_core::config::Defaults;
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let default_targets = vec![
        "x86_64-unknown-linux-gnu".to_string(),
        "aarch64-apple-darwin".to_string(),
    ];
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .defaults(Defaults {
            targets: Some(default_targets),
            ..Default::default()
        })
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: dir.path().to_str().unwrap().to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            builds: None,
            ..Default::default()
        }])
        .build();
    let targets = vec!["aarch64-apple-darwin".to_string()];
    assert!(
        matches!(
            crate::publisher_helpers::targets_allowlist_check(&ctx, Some(&targets), None, "pypi"),
            PreflightCheck::Pass
        ),
        "synthesized default build produces the allowlisted triple",
    );
}

/// An explicit empty `targets: []` reads as "publish nothing" yet the runtime
/// filter would publish everything — a config mistake, so preflight Blocks.
#[test]
fn targets_allowlist_empty_list_blocks() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    let empty: Vec<String> = Vec::new();
    match crate::publisher_helpers::targets_allowlist_check(&ctx, Some(&empty), None, "pypi") {
        PreflightCheck::Blocker(m) => {
            assert!(m.contains("pypi"), "labels the publisher: {m}");
            assert!(m.contains("empty"), "explains the empty list: {m}");
        }
        other => panic!("expected Blocker, got {other:?}"),
    }
}

/// serde round-trip: `targets:` deserializes on a `pypis[]` entry, defaults to
/// `None`, and `deny_unknown_fields` still accepts it.
#[test]
fn targets_allowlist_config_round_trip() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
pypis:
  - name: git-cliff
    targets:
      - x86_64-unknown-linux-gnu
      - x86_64-pc-windows-msvc
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse pypis targets");
    let entry = &cfg.pypis.as_ref().unwrap()[0];
    assert_eq!(
        entry.targets.as_deref(),
        Some(
            &[
                "x86_64-unknown-linux-gnu".to_string(),
                "x86_64-pc-windows-msvc".to_string()
            ][..]
        )
    );
    assert!(PypiConfig::default().targets.is_none(), "default is None");
}
