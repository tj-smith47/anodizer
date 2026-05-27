#![cfg(test)]
#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{ArchivesConfig, FormatOverride};
use anodizer_core::stage::Stage;

use crate::entries::{ArchiveEntry, deduplicate_entries, sort_entries};
use crate::file_specs::{render_file_info, resolve_default_extra_files};
use crate::formats::{
    self, copy_binary, create_gz, create_tar, create_tar_gz, create_tar_xz, create_tar_zst,
    create_xz, create_zip, resolve_glob_patterns,
};
use crate::{
    ArchiveStage, default_binary_name_template, default_name_template, format_for_target,
    formats_for_target, resolve_file_specs,
};

#[test]
fn test_create_tar_gz() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("mybin.tar.gz");
    create_tar_gz(&[&bin_path], &archive_path, None, None, None, None).unwrap();

    assert!(archive_path.exists());
    assert!(fs::metadata(&archive_path).unwrap().len() > 0);
}

#[test]
fn test_create_zip() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin.exe");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("mybin.zip");
    create_zip(&[&bin_path], &archive_path, None, None).unwrap();

    assert!(archive_path.exists());
    assert!(fs::metadata(&archive_path).unwrap().len() > 0);
}

#[test]
fn test_format_for_target() {
    assert_eq!(
        format_for_target("x86_64-unknown-linux-gnu", "tar.gz", &[]),
        "tar.gz"
    );
    assert_eq!(
        format_for_target(
            "x86_64-pc-windows-msvc",
            "tar.gz",
            &[FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]
        ),
        "zip"
    );
}

// ---------------------------------------------------------------------------
// New tests: tar.xz
// ---------------------------------------------------------------------------

#[test]
fn test_create_tar_xz() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content for xz").unwrap();

    let archive_path = tmp.path().join("mybin.tar.xz");
    create_tar_xz(&[&bin_path], &archive_path, None, None, None, None).unwrap();

    assert!(archive_path.exists());
    let len = fs::metadata(&archive_path).unwrap().len();
    assert!(len > 0, "tar.xz archive should not be empty");

    // Verify we can decompress and read the tar
    let file = File::open(&archive_path).unwrap();
    let dec = xz2::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let entries: Vec<_> = tar.entries().unwrap().collect();
    assert_eq!(entries.len(), 1);
    let entry = entries.into_iter().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
}

// ---------------------------------------------------------------------------
// New tests: tar.zst
// ---------------------------------------------------------------------------

#[test]
fn test_create_tar_zst() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content for zstd").unwrap();

    let archive_path = tmp.path().join("mybin.tar.zst");
    create_tar_zst(&[&bin_path], &archive_path, None, None, None, None).unwrap();

    assert!(archive_path.exists());
    let len = fs::metadata(&archive_path).unwrap().len();
    assert!(len > 0, "tar.zst archive should not be empty");

    // Verify we can decompress and read the tar
    let file = File::open(&archive_path).unwrap();
    let dec = zstd::Decoder::new(file).unwrap();
    let mut tar = tar::Archive::new(dec);
    let entries: Vec<_> = tar.entries().unwrap().collect();
    assert_eq!(entries.len(), 1);
    let entry = entries.into_iter().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
}

// ---------------------------------------------------------------------------
// New tests: binary format (copy)
// ---------------------------------------------------------------------------

#[test]
fn test_copy_binary_single() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("myapp");
    fs::write(&src, b"actual binary bytes").unwrap();

    let dest = tmp.path().join("dist").join("myapp");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    copy_binary(&[src.as_path()], &dest).unwrap();

    assert!(dest.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"actual binary bytes");
}

#[test]
fn test_copy_binary_multiple() {
    let tmp = TempDir::new().unwrap();
    let src1 = tmp.path().join("bin1");
    let src2 = tmp.path().join("bin2");
    fs::write(&src1, b"binary-1").unwrap();
    fs::write(&src2, b"binary-2").unwrap();

    let out_dir = tmp.path().join("dist");
    fs::create_dir_all(&out_dir).unwrap();
    let output = out_dir.join("placeholder");

    copy_binary(&[src1.as_path(), src2.as_path()], &output).unwrap();

    assert!(out_dir.join("bin1").exists());
    assert!(out_dir.join("bin2").exists());
    assert_eq!(fs::read(out_dir.join("bin1")).unwrap(), b"binary-1");
    assert_eq!(fs::read(out_dir.join("bin2")).unwrap(), b"binary-2");
}

// ---------------------------------------------------------------------------
// New tests: glob pattern resolution
// ---------------------------------------------------------------------------

/// Pins W3: a single license/readme/changelog file produces exactly one
/// resolved entry, regardless of which case glob hit it first. The dedup
/// logic in `resolve_default_extra_files` (HashSet on resolved path)
/// must collapse the two case-globs that resolve to the same file on
/// case-insensitive filesystems (macOS HFS+, Windows NTFS default).
#[test]
fn test_resolve_default_extra_files_dedup_single_file() {
    let tmp = TempDir::new().unwrap();
    // Just one license file. On both case-sensitive and case-insensitive
    // filesystems, the resolver should return exactly one entry —
    // the lowercase and uppercase globs may or may not BOTH find it,
    // but the result must be deduped.
    fs::write(tmp.path().join("license.txt"), b"mit").unwrap();
    let results = resolve_default_extra_files(tmp.path());
    assert_eq!(
        results.len(),
        1,
        "exactly one entry expected for single license file; got {results:?}"
    );
}

/// Pins C-new-6: GR-aligned default extra-file glob order is lowercase-first
/// for each of license / readme / changelog. On case-insensitive
/// filesystems where both `LICENSE` and `license` exist, the lowercase
/// glob's first match wins. Mirrors GoReleaser archive.go:84-91.
#[test]
fn test_resolve_default_extra_files_gr_aligned_lowercase_first() {
    let tmp = TempDir::new().unwrap();
    // Two distinct files with case-different basenames so the test runs
    // on case-sensitive filesystems too — there both files exist; the
    // ordering check is then about the GLOB iteration order, not
    // filesystem case folding.
    fs::write(tmp.path().join("license.txt"), b"a").unwrap();
    fs::write(tmp.path().join("LICENSE.md"), b"b").unwrap();
    fs::write(tmp.path().join("readme.txt"), b"c").unwrap();
    fs::write(tmp.path().join("README.md"), b"d").unwrap();
    fs::write(tmp.path().join("changelog.txt"), b"e").unwrap();
    fs::write(tmp.path().join("CHANGELOG.md"), b"f").unwrap();

    let results = resolve_default_extra_files(tmp.path());
    let names: Vec<String> = results
        .iter()
        .filter_map(|r| {
            r.src
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .collect();

    // Each of the three lowercase basename should appear before its
    // uppercase counterpart in the resolved order.
    let pos = |name: &str| {
        names
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{name} missing from resolved list: {names:?}"))
    };
    assert!(
        pos("license.txt") < pos("LICENSE.md"),
        "license.txt must precede LICENSE.md (got {names:?})"
    );
    assert!(
        pos("readme.txt") < pos("README.md"),
        "readme.txt must precede README.md (got {names:?})"
    );
    assert!(
        pos("changelog.txt") < pos("CHANGELOG.md"),
        "changelog.txt must precede CHANGELOG.md (got {names:?})"
    );
}

/// Regression: auto-resolved LICENSE/README/CHANGELOG entries must carry
/// `default: true` so diagnostics can distinguish them from user-specified
/// `files:`. User-configured globs keep `default: false`.
#[test]
fn test_resolve_default_extra_files_marks_default_flag() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("LICENSE"), b"mit").unwrap();
    fs::write(tmp.path().join("README.md"), b"# readme").unwrap();

    let results = resolve_default_extra_files(tmp.path());

    assert!(
        !results.is_empty(),
        "LICENSE and README should be picked up"
    );
    for r in &results {
        assert!(
            r.default,
            "auto-resolved default file must set default=true: {:?}",
            r.src
        );
    }
}

/// Regression: when no extra files are configured, the default-file glob
/// must scope to the crate's directory — never the process CWD. Otherwise
/// running `cargo test` from the workspace root leaks the workspace's
/// top-level README/LICENSE/CHANGELOG into per-crate archives.
#[test]
fn test_resolve_default_extra_files_does_not_leak_cwd() {
    let tmp = TempDir::new().unwrap();
    // crate_dir intentionally has NO LICENSE/README/CHANGELOG.
    let crate_dir = tmp.path().join("empty_crate");
    fs::create_dir(&crate_dir).unwrap();

    let results = resolve_default_extra_files(&crate_dir);
    assert!(
        results.is_empty(),
        "must not glob outside the crate dir; got: {:?}",
        results.iter().map(|r| &r.src).collect::<Vec<_>>()
    );
}

#[test]
fn test_resolve_glob_patterns() {
    let tmp = TempDir::new().unwrap();
    let license = tmp.path().join("LICENSE");
    let license_mit = tmp.path().join("LICENSE-MIT");
    let readme = tmp.path().join("README.md");
    fs::write(&license, b"license").unwrap();
    fs::write(&license_mit, b"mit license").unwrap();
    fs::write(&readme, b"readme").unwrap();

    let pattern = format!("{}/*", tmp.path().display());
    let results = resolve_glob_patterns(&[pattern]).unwrap();
    assert!(
        results.len() >= 3,
        "should match at least 3 files, got {}",
        results.len()
    );

    // Test with LICENSE* glob
    let license_pattern = format!("{}/LICENSE*", tmp.path().display());
    let results = resolve_glob_patterns(&[license_pattern]).unwrap();
    assert_eq!(results.len(), 2, "LICENSE* should match 2 files");
    assert!(results.iter().any(|p| p.file_name().unwrap() == "LICENSE"));
    assert!(
        results
            .iter()
            .any(|p| p.file_name().unwrap() == "LICENSE-MIT")
    );
}

#[test]
fn test_resolve_glob_patterns_literal() {
    let tmp = TempDir::new().unwrap();
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"license content").unwrap();

    // A literal (non-glob) path that exists should be returned
    let results = resolve_glob_patterns(&[license.to_string_lossy().to_string()]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], license);

    // A literal path that does not exist should be silently skipped
    let results = resolve_glob_patterns(&["/nonexistent/file".to_string()]).unwrap();
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// New tests: wrap_in_directory
// ---------------------------------------------------------------------------

#[test]
fn test_wrap_in_directory_tar_gz() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    let license_path = tmp.path().join("LICENSE");
    fs::write(&bin_path, b"binary").unwrap();
    fs::write(&license_path, b"MIT").unwrap();

    let archive_path = tmp.path().join("wrapped.tar.gz");
    create_tar_gz(
        &[&bin_path, &license_path],
        &archive_path,
        None,
        Some("myapp-1.0.0"),
        None,
        None,
    )
    .unwrap();

    // Verify entries have the directory prefix
    let file = File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let mut names: Vec<String> = Vec::new();
    for entry in tar.entries().unwrap() {
        let entry = entry.unwrap();
        names.push(entry.path().unwrap().to_string_lossy().to_string());
    }
    names.sort();
    assert_eq!(names.len(), 2);
    assert_eq!(names[0], "myapp-1.0.0/LICENSE");
    assert_eq!(names[1], "myapp-1.0.0/mybin");
}

#[test]
fn test_wrap_in_directory_zip() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin.exe");
    fs::write(&bin_path, b"binary").unwrap();

    let archive_path = tmp.path().join("wrapped.zip");
    create_zip(&[&bin_path], &archive_path, Some("myapp-1.0.0"), None).unwrap();

    // Verify entry has the directory prefix
    let file = File::open(&archive_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    assert_eq!(zip.len(), 1);
    let entry = zip.by_index(0).unwrap();
    assert_eq!(entry.name(), "myapp-1.0.0/mybin.exe");
}

#[test]
fn test_wrap_in_directory_tar_xz() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary").unwrap();

    let archive_path = tmp.path().join("wrapped.tar.xz");
    create_tar_xz(
        &[&bin_path],
        &archive_path,
        None,
        Some("myapp-1.0.0"),
        None,
        None,
    )
    .unwrap();

    // Verify entry has the directory prefix
    let file = File::open(&archive_path).unwrap();
    let dec = xz2::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let entries: Vec<_> = tar.entries().unwrap().collect();
    assert_eq!(entries.len(), 1);
    let entry = entries.into_iter().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_str().unwrap(), "myapp-1.0.0/mybin");
}

#[test]
fn test_wrap_in_directory_tar_zst() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary").unwrap();

    let archive_path = tmp.path().join("wrapped.tar.zst");
    create_tar_zst(
        &[&bin_path],
        &archive_path,
        None,
        Some("myapp-1.0.0"),
        None,
        None,
    )
    .unwrap();

    // Verify entry has the directory prefix
    let file = File::open(&archive_path).unwrap();
    let dec = zstd::Decoder::new(file).unwrap();
    let mut tar = tar::Archive::new(dec);
    let entries: Vec<_> = tar.entries().unwrap().collect();
    assert_eq!(entries.len(), 1);
    let entry = entries.into_iter().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_str().unwrap(), "myapp-1.0.0/mybin");
}

// ---------------------------------------------------------------------------
// Config parsing test for wrap_in_directory
// ---------------------------------------------------------------------------

#[test]
fn test_archive_config_parses_wrap_in_directory() {
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - formats: [tar.gz]
        wrap_in_directory: "myapp-{{ .Version }}"
        files:
          - LICENSE
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    match &config.crates[0].archives {
        ArchivesConfig::Configs(cfgs) => {
            assert_eq!(cfgs.len(), 1);
            assert_eq!(
                cfgs[0].wrap_in_directory,
                Some(anodizer_core::config::WrapInDirectory::Name(
                    "myapp-{{ .Version }}".to_string()
                ))
            );
            assert_eq!(
                cfgs[0].formats.as_deref(),
                Some(&["tar.gz".to_string()][..])
            );
        }
        _ => panic!("expected Configs variant"),
    }
}

// ---------------------------------------------------------------------------
// Integration-style test: ArchiveStage.run
// ---------------------------------------------------------------------------

#[test]
fn archive_name_template_empty_bails_with_actionable_error() {
    // A `name_template:` that renders to an empty string would produce
    // `dist/.tar.gz` (a hidden file) which the duplicate-name detector
    // and downstream stages cannot resolve. The stage must bail with
    // an actionable hint that names the crate/target context.
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(String::new()),
                formats: Some(vec!["tar.gz".to_string()]),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    let err = stage
        .run(&mut ctx)
        .expect_err("empty archive name template must bail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("archive:"),
        "error must carry the archive: prefix, got: {chain}"
    );
    assert!(
        chain.contains("empty stem"),
        "error must describe the empty-stem condition, got: {chain}"
    );
    assert!(
        chain.contains("myapp"),
        "error must name the crate context, got: {chain}"
    );
    assert!(
        chain.contains("name_template") || chain.contains("snapshot"),
        "error must include an actionable hint, got: {chain}"
    );
}

#[test]
fn test_archive_stage_run() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create a fake binary
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                formats: Some(vec!["tar.gz".to_string()]),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    // Register a Binary artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    // Should have registered one Archive artifact
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert!(archives[0].path.exists());
    assert!(archives[0].path.to_string_lossy().ends_with(".tar.gz"));
}

/// Pins C-new-4: when `archives: []` (or omitted) is set on a crate WITH
/// builds, the stage auto-injects `ArchiveConfig::default()` so the user
/// still gets a default `tar.gz`. Mirrors GoReleaser archive.go:57-59:
/// `if len(ctx.Config.Archives) == 0 { Archives = append(Archives, Archive{}) }`.
#[test]
fn test_archive_stage_empty_archives_auto_injects_default() {
    use anodizer_core::config::{ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            // Empty archives list — must auto-inject the default.
            archives: ArchivesConfig::Configs(vec![]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m.insert("id".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(
        archives.len(),
        1,
        "auto-injected default archive must produce one .tar.gz"
    );
    assert!(
        archives[0].path.to_string_lossy().ends_with(".tar.gz"),
        "auto-injected default format is tar.gz, got {}",
        archives[0].path.display()
    );
}

#[test]
fn test_archive_stage_disabled() {
    use anodizer_core::config::{ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Disabled,
            ..Default::default()
        }])
        .build();

    // Register a Binary artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/fake/path"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    // No archives should be registered
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert!(archives.is_empty());
}

#[test]
fn test_archive_stage_zip_for_windows() {
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, Config, CrateConfig, FormatOverride,
    };
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp.exe");
    fs::write(&bin_path, b"fake windows binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: Some(vec![FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]),
            files: None,
            binaries: None,
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert!(archives[0].path.to_string_lossy().ends_with(".zip"));
    assert!(archives[0].path.exists());
}

// ---------------------------------------------------------------------------
// Integration test: ArchiveStage with tar.xz format
// ---------------------------------------------------------------------------

#[test]
fn test_archive_stage_tar_xz_format() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary for xz").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["tar.xz".to_string()]),
            format_overrides: None,
            files: None,
            binaries: None,
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert!(archives[0].path.to_string_lossy().ends_with(".tar.xz"));
    assert!(archives[0].path.exists());
    assert!(fs::metadata(&archives[0].path).unwrap().len() > 0);
}

// ---------------------------------------------------------------------------
// Integration test: ArchiveStage with binary format
// ---------------------------------------------------------------------------

#[test]
fn test_archive_stage_binary_format() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"raw binary content").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["binary".to_string()]),
            format_overrides: None,
            files: None,
            binaries: None,
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    // `format: binary` registers UploadableBinary artifacts, one per source binary
    // (matches GoReleaser archive.go:143-145,296-336).
    let bins = ctx.artifacts.by_kind(ArtifactKind::UploadableBinary);
    assert_eq!(bins.len(), 1);
    let name = bins[0].path.file_name().unwrap().to_str().unwrap();
    assert!(!name.contains(".tar"));
    assert!(!name.contains(".zip"));
    assert!(!name.contains(".gz"));
    assert!(bins[0].path.exists());
    assert_eq!(fs::read(&bins[0].path).unwrap(), b"raw binary content");
}

// -----------------------------------------------------------------------
// Deep integration tests: realistic file trees, verify archive contents
// -----------------------------------------------------------------------

/// Helper: read all entries from a tar archive into a HashMap of name -> content.
fn read_tar_entries<R: std::io::Read>(archive: tar::Archive<R>) -> HashMap<String, Vec<u8>> {
    let mut found_files = HashMap::new();
    let mut archive = archive;
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().to_string();
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
        found_files.insert(name, content);
    }
    found_files
}

/// Helper: create a realistic file tree with a binary, LICENSE, and README.
fn create_realistic_file_tree(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let bin = dir.join("myapp");
    let license = dir.join("LICENSE");
    let readme = dir.join("README.md");
    fs::write(
        &bin,
        b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03",
    )
    .unwrap();
    fs::write(
        &license,
        b"MIT License\n\nCopyright (c) 2026 Example Corp\n\nPermission is hereby granted...",
    )
    .unwrap();
    fs::write(
        &readme,
        b"# MyApp\n\nA tool for doing things.\n\n## Usage\n\n```\nmyapp --help\n```\n",
    )
    .unwrap();
    (bin, license, readme)
}

#[test]
fn test_integration_tar_gz_realistic_file_tree() {
    let tmp = TempDir::new().unwrap();
    let (bin, license, readme) = create_realistic_file_tree(tmp.path());

    let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.gz");
    create_tar_gz(
        &[&bin, &license, &readme],
        &archive_path,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // Open the archive and verify all files are present with correct names
    let file = File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));

    assert_eq!(
        found_files.len(),
        3,
        "archive should contain exactly 3 files"
    );
    assert!(
        found_files.contains_key("myapp"),
        "should contain myapp binary"
    );
    assert!(
        found_files.contains_key("LICENSE"),
        "should contain LICENSE"
    );
    assert!(
        found_files.contains_key("README.md"),
        "should contain README.md"
    );

    // Verify file contents are preserved byte-for-byte
    assert_eq!(
        found_files["myapp"],
        b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
        "binary content should be preserved exactly"
    );
    assert!(
        found_files["LICENSE"].starts_with(b"MIT License"),
        "LICENSE content should be preserved"
    );
    assert!(
        found_files["README.md"].starts_with(b"# MyApp"),
        "README content should be preserved"
    );
}

#[test]
fn test_integration_zip_realistic_file_tree() {
    let tmp = TempDir::new().unwrap();
    let (bin, license, readme) = create_realistic_file_tree(tmp.path());

    let archive_path = tmp.path().join("myapp-1.0.0-windows-amd64.zip");
    create_zip(&[&bin, &license, &readme], &archive_path, None, None).unwrap();

    // Open the zip and verify all files
    let file = File::open(&archive_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();

    assert_eq!(zip.len(), 3, "zip should contain exactly 3 files");

    let mut found_names: Vec<String> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i).unwrap();
        found_names.push(entry.name().to_string());
    }
    found_names.sort();
    assert_eq!(found_names, vec!["LICENSE", "README.md", "myapp"]);

    // Verify binary content is preserved
    {
        let mut bin_entry = zip.by_name("myapp").unwrap();
        let mut bin_content = Vec::new();
        std::io::Read::read_to_end(&mut bin_entry, &mut bin_content).unwrap();
        assert_eq!(
            bin_content,
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
            "binary content in zip should be preserved exactly"
        );
    }

    // Verify LICENSE content is preserved
    {
        let mut lic_entry = zip.by_name("LICENSE").unwrap();
        let mut lic_content = Vec::new();
        std::io::Read::read_to_end(&mut lic_entry, &mut lic_content).unwrap();
        assert!(lic_content.starts_with(b"MIT License"));
    }
}

#[test]
fn test_integration_tar_xz_realistic_file_tree() {
    let tmp = TempDir::new().unwrap();
    let (bin, license, readme) = create_realistic_file_tree(tmp.path());

    let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.xz");
    create_tar_xz(
        &[&bin, &license, &readme],
        &archive_path,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // Open the archive and verify all files
    let file = File::open(&archive_path).unwrap();
    let dec = xz2::read::XzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));

    assert_eq!(
        found_files.len(),
        3,
        "tar.xz should contain exactly 3 files"
    );
    assert!(found_files.contains_key("myapp"));
    assert!(found_files.contains_key("LICENSE"));
    assert!(found_files.contains_key("README.md"));

    // Verify binary content
    assert_eq!(
        found_files["myapp"],
        b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
        "binary content in tar.xz should be preserved exactly"
    );

    // Verify text content
    let readme_str = String::from_utf8(found_files["README.md"].clone()).unwrap();
    assert!(
        readme_str.contains("## Usage"),
        "README structure should be intact"
    );
    assert!(
        readme_str.contains("myapp --help"),
        "README content should be preserved"
    );
}

#[test]
fn test_integration_tar_gz_with_wrap_dir_contents_verified() {
    let tmp = TempDir::new().unwrap();
    let (bin, license, readme) = create_realistic_file_tree(tmp.path());

    let archive_path = tmp.path().join("myapp-1.0.0.tar.gz");
    create_tar_gz(
        &[&bin, &license, &readme],
        &archive_path,
        None,
        Some("myapp-1.0.0"),
        None,
        None,
    )
    .unwrap();

    let file = File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));

    // All entries should be prefixed with wrap directory
    assert_eq!(found_files.len(), 3);
    assert!(found_files.contains_key("myapp-1.0.0/myapp"));
    assert!(found_files.contains_key("myapp-1.0.0/LICENSE"));
    assert!(found_files.contains_key("myapp-1.0.0/README.md"));

    // Contents still preserved after wrapping
    assert_eq!(
        found_files["myapp-1.0.0/myapp"],
        b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec()
    );
}

// -----------------------------------------------------------------------
// Integration test: tar.zst with realistic file tree
// -----------------------------------------------------------------------

#[test]
fn test_integration_tar_zst_realistic_file_tree() {
    let tmp = TempDir::new().unwrap();
    let (bin, license, readme) = create_realistic_file_tree(tmp.path());

    let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.zst");
    create_tar_zst(
        &[&bin, &license, &readme],
        &archive_path,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // Open the archive and verify all files
    let file = File::open(&archive_path).unwrap();
    let dec = zstd::Decoder::new(file).unwrap();
    let found_files = read_tar_entries(tar::Archive::new(dec));

    assert_eq!(
        found_files.len(),
        3,
        "tar.zst should contain exactly 3 files"
    );
    assert!(
        found_files.contains_key("myapp"),
        "should contain myapp binary"
    );
    assert!(
        found_files.contains_key("LICENSE"),
        "should contain LICENSE"
    );
    assert!(
        found_files.contains_key("README.md"),
        "should contain README.md"
    );

    // Verify binary content is preserved byte-for-byte
    assert_eq!(
        found_files["myapp"],
        b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
        "binary content in tar.zst should be preserved exactly"
    );

    // Verify text content
    assert!(
        found_files["LICENSE"].starts_with(b"MIT License"),
        "LICENSE content should be preserved"
    );
    let readme_str = String::from_utf8(found_files["README.md"].clone()).unwrap();
    assert!(
        readme_str.contains("## Usage"),
        "README structure should be intact"
    );
    assert!(
        readme_str.contains("myapp --help"),
        "README content should be preserved"
    );
}

// -- TestContextBuilder integration test: verify stage-scoped vars --

#[test]
fn test_archive_stage_scoped_vars_not_preset() {
    use anodizer_core::test_helpers::TestContextBuilder;

    let ctx = TestContextBuilder::new()
        .project_name("archive-test")
        .tag("v1.0.0")
        .build();

    // Os and Arch are stage-scoped — they should NOT be set by the builder.
    // The archive stage sets them per-target during execution.
    assert_eq!(ctx.template_vars().get("Os"), None);
    assert_eq!(ctx.template_vars().get("Arch"), None);

    // But project-level vars should be present
    assert_eq!(
        ctx.template_vars().get("ProjectName"),
        Some(&"archive-test".to_string())
    );
    assert_eq!(
        ctx.template_vars().get("Version"),
        Some(&"1.0.0".to_string())
    );
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_format_for_target_multiple_overrides() {
    // Multiple OS overrides: windows->zip AND darwin->tar.gz
    let overrides = vec![
        FormatOverride {
            os: "windows".to_string(),
            formats: Some(vec!["zip".to_string()]),
        },
        FormatOverride {
            os: "darwin".to_string(),
            formats: Some(vec!["tar.gz".to_string()]),
        },
    ];
    // Default is tar.xz but windows should get zip
    assert_eq!(
        format_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides),
        "zip"
    );
    // darwin should get tar.gz
    assert_eq!(
        format_for_target("aarch64-apple-darwin", "tar.xz", &overrides),
        "tar.gz"
    );
    // Linux falls through to default
    assert_eq!(
        format_for_target("x86_64-unknown-linux-gnu", "tar.xz", &overrides),
        "tar.xz"
    );
}

#[test]
fn test_archive_stage_multiple_binaries_per_archive() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create two fake binaries for the same target
    let bin1 = tmp.path().join("myapp");
    let bin2 = tmp.path().join("myhelper");
    fs::write(&bin1, b"binary 1").unwrap();
    fs::write(&bin2, b"binary 2").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: None,
            files: None,
            binaries: None, // Include all binaries
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    // Register two binary artifacts for the same target
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin1.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin2.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myhelper".to_string());
            m
        },
        size: None,
    });

    // Pin project_root to the test tmp dir so default-extra-files glob
    // (LICENSE/README/CHANGELOG) doesn't pick up the workspace's own
    // files when `cargo test` runs from /opt/repos/anodizer.
    ctx.options.project_root = Some(tmp.path().to_path_buf());

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    // Should create one archive containing both binaries
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert!(archives[0].path.exists());

    // Verify both binaries are in the archive
    let file = File::open(&archives[0].path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));
    let names: Vec<&String> = found_files.keys().collect();
    assert_eq!(
        found_files.len(),
        2,
        "archive should contain both binaries; got: {names:?}"
    );
    assert!(found_files.contains_key("myapp"));
    assert!(found_files.contains_key("myhelper"));
}

#[test]
fn test_archive_stage_default_config_inheritance() {
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, Config, CrateConfig, Defaults, FormatOverride,
    };
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp.exe");
    fs::write(&bin, b"fake windows binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        // Use default archive config (no format_overrides set) — should inherit global
        archives: ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];
    // Global defaults: format_overrides windows -> zip
    config.defaults = Some(Defaults {
        archives: Some(ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: Some(vec![FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    });

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin.clone(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    // Should have inherited global format_override: windows -> zip
    assert!(
        archives[0].path.to_string_lossy().ends_with(".zip"),
        "windows archive should use zip from global defaults, got: {}",
        archives[0].path.display()
    );
}

#[test]
fn test_archive_stage_name_template_renders_all_variables() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"fake binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: None,
            files: None,
            binaries: None,
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "2.5.0");
    ctx.template_vars_mut().set("Tag", "v2.5.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin.clone(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);

    let name = archives[0].path.file_name().unwrap().to_str().unwrap();
    assert_eq!(name, "myapp_2.5.0_darwin_arm64.tar.gz");
}

#[test]
fn test_archive_stage_files_included_alongside_binaries() {
    use anodizer_core::config::{
        ArchiveConfig, ArchiveFileSpec, ArchivesConfig, Config, CrateConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    let license = tmp.path().join("LICENSE");
    let readme = tmp.path().join("README.md");
    fs::write(&bin, b"binary content").unwrap();
    fs::write(&license, b"MIT License").unwrap();
    fs::write(&readme, b"# MyApp").unwrap();

    let license_pattern = tmp.path().join("LICENSE").to_string_lossy().to_string();
    let readme_pattern = tmp.path().join("README.md").to_string_lossy().to_string();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some("myapp-1.0.0-linux-amd64".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: None,
            files: Some(vec![
                ArchiveFileSpec::Glob(license_pattern),
                ArchiveFileSpec::Glob(readme_pattern),
            ]),
            binaries: None,
            wrap_in_directory: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);

    // Verify all 3 files are in the archive
    let file = File::open(&archives[0].path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));
    assert_eq!(
        found_files.len(),
        3,
        "archive should contain binary + 2 extra files"
    );
    assert!(found_files.contains_key("myapp"));
    assert!(found_files.contains_key("LICENSE"));
    assert!(found_files.contains_key("README.md"));
}

#[test]
fn test_archive_registers_correct_metadata() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["zip".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    // Verify the artifact metadata contains format and name
    assert_eq!(archives[0].metadata.get("format"), Some(&"zip".to_string()));
    assert!(archives[0].metadata.contains_key("name"));
    // Verify it's registered as an Archive artifact for the right crate
    assert_eq!(archives[0].crate_name, "myapp");
    assert_eq!(archives[0].kind, ArtifactKind::Archive);
    // Target should be preserved
    assert_eq!(
        archives[0].target.as_deref(),
        Some("x86_64-pc-windows-msvc")
    );
}

#[test]
fn test_archive_stage_wrap_in_directory_renders_template() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some("myapp-linux-amd64".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            wrap_in_directory: Some(anodizer_core::config::WrapInDirectory::Name(
                "{{ .ProjectName }}-{{ .Version }}".to_string(),
            )),
            files: None,
            format_overrides: None,
            binaries: None,
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "3.0.0");
    ctx.template_vars_mut().set("Tag", "v3.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);

    // Verify that the wrap directory was rendered from the template
    let file = File::open(&archives[0].path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found = read_tar_entries(tar::Archive::new(dec));
    assert!(
        found.contains_key("myapp-3.0.0/myapp"),
        "wrap directory should use rendered template, got keys: {:?}",
        found.keys().collect::<Vec<_>>()
    );
}

// ---- Error path tests: missing binary / archive failures ----

#[test]
fn test_missing_binary_artifact_errors_with_path() {
    use anodizer_core::config::{ArchiveConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: anodizer_core::config::ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("ProjectName", "myapp");

    // Register a binary artifact that doesn't exist on disk
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/nonexistent/path/to/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let result = ArchiveStage.run(&mut ctx);
    assert!(result.is_err(), "archive with missing binary should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("binary artifact missing") || err.contains("/nonexistent/path/to/myapp"),
        "error should mention the missing binary path, got: {err}"
    );
}

#[test]
fn test_empty_file_list_creates_empty_tar_gz() {
    let tmp = TempDir::new().unwrap();
    let archive_path = tmp.path().join("empty.tar.gz");

    // Create an archive with empty file list
    let result = create_tar_gz(&[], &archive_path, None, None, None, None);
    assert!(
        result.is_ok(),
        "creating archive with empty file list should succeed"
    );
    assert!(archive_path.exists(), "archive file should be created");
}

#[test]
fn test_empty_file_list_creates_empty_zip() {
    let tmp = TempDir::new().unwrap();
    let archive_path = tmp.path().join("empty.zip");

    let result = create_zip(&[], &archive_path, None, None);
    assert!(
        result.is_ok(),
        "creating zip with empty file list should succeed"
    );
    assert!(archive_path.exists(), "zip file should be created");
}

#[test]
fn test_copy_binary_source_missing_errors_with_path() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let output = tmp.path().join("output");

    let result = copy_binary(&[missing.as_path()], &output);
    assert!(
        result.is_err(),
        "copy_binary with missing source should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not exist") || err.contains("does-not-exist"),
        "error should mention the missing file, got: {err}"
    );
}

#[test]
fn test_archive_unsupported_format_returns_error() {
    // Unknown archive formats should produce a clear error.
    use anodizer_core::config::{ArchiveConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"fake binary").unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: anodizer_core::config::ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["unsupported_format".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("binary".to_string(), "mybin".to_string());
            m
        },
        size: None,
    });

    let result = ArchiveStage.run(&mut ctx);
    assert!(result.is_err(), "unsupported format should return an error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unsupported archive format"),
        "error should mention 'unsupported archive format', got: {err}"
    );
}

// ---- Reproducible archive mtime tests ----

#[test]
fn test_create_tar_gz_with_fixed_mtime() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("mybin-reproducible.tar.gz");
    let fixed_mtime: u64 = 1_700_000_000;
    create_tar_gz(
        &[&bin_path],
        &archive_path,
        None,
        None,
        Some(fixed_mtime),
        None,
    )
    .unwrap();

    assert!(archive_path.exists());

    // Verify the stored mtime matches the fixed timestamp
    let file = File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    assert_eq!(
        entry.header().mtime().unwrap(),
        fixed_mtime,
        "tar.gz entry mtime should match SOURCE_DATE_EPOCH"
    );
}

#[test]
fn test_create_tar_xz_with_fixed_mtime() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("mybin-reproducible.tar.xz");
    let fixed_mtime: u64 = 1_700_000_000;
    create_tar_xz(
        &[&bin_path],
        &archive_path,
        None,
        None,
        Some(fixed_mtime),
        None,
    )
    .unwrap();

    assert!(archive_path.exists());

    let file = File::open(&archive_path).unwrap();
    let dec = xz2::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    assert_eq!(
        entry.header().mtime().unwrap(),
        fixed_mtime,
        "tar.xz entry mtime should match SOURCE_DATE_EPOCH"
    );
}

#[test]
fn test_create_tar_zst_with_fixed_mtime() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("mybin-reproducible.tar.zst");
    let fixed_mtime: u64 = 1_700_000_000;
    create_tar_zst(
        &[&bin_path],
        &archive_path,
        None,
        None,
        Some(fixed_mtime),
        None,
    )
    .unwrap();

    assert!(archive_path.exists());

    let file = File::open(&archive_path).unwrap();
    let dec = zstd::Decoder::new(file).unwrap();
    let mut tar = tar::Archive::new(dec);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    assert_eq!(
        entry.header().mtime().unwrap(),
        fixed_mtime,
        "tar.zst entry mtime should match SOURCE_DATE_EPOCH"
    );
}

#[test]
fn test_reproducible_archive_is_deterministic() {
    // Two archives created with the same content and fixed mtime must be byte-identical
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"deterministic binary content").unwrap();

    let fixed_mtime: u64 = 1_700_000_000;
    let archive1 = tmp.path().join("archive1.tar.gz");
    let archive2 = tmp.path().join("archive2.tar.gz");

    create_tar_gz(&[&bin_path], &archive1, None, None, Some(fixed_mtime), None).unwrap();
    create_tar_gz(&[&bin_path], &archive2, None, None, Some(fixed_mtime), None).unwrap();

    let bytes1 = fs::read(&archive1).unwrap();
    let bytes2 = fs::read(&archive2).unwrap();
    assert_eq!(
        bytes1, bytes2,
        "archives with same content and fixed mtime should be byte-identical"
    );
}

// -----------------------------------------------------------------------
// ids filtering tests
// -----------------------------------------------------------------------

#[test]
fn test_archive_ids_filter_only_matching_builds() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create two fake binaries: one with id "linux-build", one with id "windows-build"
    let linux_bin = tmp.path().join("myapp-linux");
    let windows_bin = tmp.path().join("myapp-windows");
    fs::write(&linux_bin, b"linux binary").unwrap();
    fs::write(&windows_bin, b"windows binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                ids: Some(vec!["linux-build".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    // Register binaries with different build IDs
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: linux_bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([
            ("binary".to_string(), "myapp".to_string()),
            ("id".to_string(), "linux-build".to_string()),
        ]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: windows_bin,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([
            ("binary".to_string(), "myapp".to_string()),
            ("id".to_string(), "windows-build".to_string()),
        ]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(
        archives.len(),
        1,
        "only one archive should be created (linux-build only)"
    );
    assert!(
        archives[0].target.as_deref().unwrap().contains("linux"),
        "archive should be for the linux target"
    );
}

#[test]
fn test_archive_ids_filter_excludes_all_when_no_match() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                ids: Some(vec!["nonexistent-id".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([
            ("binary".to_string(), "myapp".to_string()),
            ("id".to_string(), "some-other-id".to_string()),
        ]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert!(
        archives.is_empty(),
        "no archives should be created when ids filter matches nothing"
    );
}

#[test]
fn test_archive_ids_filter_none_includes_all() {
    // When ids is None, all binaries should be included (backward compat)
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let linux_bin = tmp.path().join("myapp-linux");
    let win_bin = tmp.path().join("myapp-win");
    fs::write(&linux_bin, b"linux binary").unwrap();
    fs::write(&win_bin, b"windows binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                // ids is None (default) — all binaries should be included
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: linux_bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([
            ("binary".to_string(), "myapp".to_string()),
            ("id".to_string(), "linux-build".to_string()),
        ]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: win_bin,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([
            ("binary".to_string(), "myapp".to_string()),
            ("id".to_string(), "windows-build".to_string()),
        ]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(
        archives.len(),
        2,
        "both targets should produce archives when ids is None"
    );
}

// -----------------------------------------------------------------------
// id metadata tests
// -----------------------------------------------------------------------

#[test]
fn test_archive_id_metadata_propagated() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                id: Some("linux-archive".to_string()),
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(
        archives[0].metadata.get("id"),
        Some(&"linux-archive".to_string()),
        "archive artifact should have the config id in metadata"
    );
}

#[test]
fn test_archive_id_metadata_absent_when_not_set() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                // id is None (default)
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    // GoReleaser archive Default() sets id="default" when empty so
    // downstream `ids:` filters can match unlabeled archives. The
    // metadata reflects that effective id.
    assert_eq!(
        archives[0].metadata.get("id").map(String::as_str),
        Some("default"),
        "archive artifact metadata id should default to \"default\" when config id is None"
    );
}

// -----------------------------------------------------------------------
// formats (plural) tests
// -----------------------------------------------------------------------

#[test]
fn test_archive_formats_plural_produces_multiple_archives() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string(), "zip".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(
        archives.len(),
        2,
        "should produce one archive per format in formats list"
    );

    let mut formats: Vec<String> = archives
        .iter()
        .map(|a| a.metadata.get("format").unwrap().clone())
        .collect();
    formats.sort();
    assert_eq!(formats, vec!["tar.gz", "zip"]);

    // Both archives should exist on disk
    for a in &archives {
        assert!(
            a.path.exists(),
            "archive should exist: {}",
            a.path.display()
        );
    }

    // Verify file extensions
    let paths: Vec<String> = archives
        .iter()
        .map(|a| a.path.to_string_lossy().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with(".tar.gz")),
        "should have a tar.gz archive"
    );
    assert!(
        paths.iter().any(|p| p.ends_with(".zip")),
        "should have a zip archive"
    );
}

#[test]
fn test_archive_formats_plural_ignores_singular_format() {
    // When formats (plural) is set, singular format should be ignored
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string(), "zip".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 2);

    let mut formats: Vec<String> = archives
        .iter()
        .map(|a| a.metadata.get("format").unwrap().clone())
        .collect();
    formats.sort();
    assert_eq!(
        formats,
        vec!["tar.gz", "zip"],
        "should use formats (plural), not singular format"
    );
}

// ---------------------------------------------------------------------------
// Uncompressed tar archive
// ---------------------------------------------------------------------------

#[test]
fn test_create_tar_uncompressed() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content for tar").unwrap();

    let archive_path = tmp.path().join("mybin.tar");
    create_tar(&[&bin_path], &archive_path, None, None, None, None).unwrap();

    assert!(archive_path.exists());
    let len = fs::metadata(&archive_path).unwrap().len();
    assert!(len > 0, "uncompressed tar archive should not be empty");

    // Verify we can read the tar directly (no decompression needed)
    let file = File::open(&archive_path).unwrap();
    let mut tar = tar::Archive::new(file);
    let entries: Vec<_> = tar.entries().unwrap().collect();
    assert_eq!(entries.len(), 1);
    let entry = entries.into_iter().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
}

// ---------------------------------------------------------------------------
// Format alias tests: tgz, txz, tzst, tar via stage
// ---------------------------------------------------------------------------

#[test]
fn test_archive_stage_tgz_alias() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tgz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0].metadata.get("format"), Some(&"tgz".to_string()));
    assert!(archives[0].path.exists(), "tgz archive file should exist");
}

#[test]
fn test_archive_stage_txz_alias() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["txz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0].metadata.get("format"), Some(&"txz".to_string()));
    assert!(archives[0].path.exists(), "txz archive file should exist");
}

#[test]
fn test_archive_stage_tzst_alias() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tzst".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(
        archives[0].metadata.get("format"),
        Some(&"tzst".to_string())
    );
    assert!(archives[0].path.exists(), "tzst archive file should exist");
}

#[test]
fn test_archive_stage_uncompressed_tar() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0].metadata.get("format"), Some(&"tar".to_string()));
    assert!(archives[0].path.exists(), "tar archive file should exist");
}

#[test]
fn test_archive_stage_unknown_format_errors() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["rar".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let result = ArchiveStage.run(&mut ctx);
    assert!(result.is_err(), "unknown format should produce an error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unsupported archive format") && err.contains("rar"),
        "error should mention the unsupported format, got: {err}"
    );
}

// -----------------------------------------------------------------------
// Config parsing tests for parity features
// -----------------------------------------------------------------------

#[test]
fn test_config_parse_archive_file_spec_glob() {
    use anodizer_core::config::{ArchiveFileSpec, Config};
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - files:
          - LICENSE*
          - README.md
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
        let files = cfgs[0].files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "LICENSE*");
        assert_eq!(files[1], "README.md");
        // Verify it deserialized as Glob variant
        assert!(matches!(&files[0], ArchiveFileSpec::Glob(_)));
    } else {
        panic!("expected Configs variant");
    }
}

#[test]
fn test_config_parse_archive_file_spec_detailed() {
    use anodizer_core::config::{ArchiveFileSpec, Config};
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - files:
          - src: "LICENSE*"
            dst: "licenses/"
            info:
              owner: root
              group: root
              mode: 0o644
          - src: "completions/*"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
        let files = cfgs[0].files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        match &files[0] {
            ArchiveFileSpec::Detailed { src, dst, info, .. } => {
                assert_eq!(src, "LICENSE*");
                assert_eq!(dst.as_deref(), Some("licenses/"));
                let info = info.as_ref().unwrap();
                assert_eq!(info.owner.as_deref(), Some("root"));
                assert_eq!(info.group.as_deref(), Some("root"));
                assert_eq!(info.mode, Some(anodizer_core::config::StringOrU32(0o644)));
            }
            _ => panic!("expected Detailed variant for first entry"),
        }
        match &files[1] {
            ArchiveFileSpec::Detailed { src, dst, info, .. } => {
                assert_eq!(src, "completions/*");
                assert!(dst.is_none());
                assert!(info.is_none());
            }
            _ => panic!("expected Detailed variant for second entry"),
        }
    } else {
        panic!("expected Configs variant");
    }
}

#[test]
fn test_config_parse_format_override_formats_plural() {
    use anodizer_core::config::Config;
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - format_overrides:
          - os: windows
            formats:
              - zip
              - tar.gz
          - os: darwin
            formats: [tar.xz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
        let overrides = cfgs[0].format_overrides.as_ref().unwrap();
        assert_eq!(overrides.len(), 2);
        // First override: windows with multiple formats
        assert_eq!(overrides[0].os, "windows");
        let fmts = overrides[0].formats.as_ref().unwrap();
        assert_eq!(fmts, &["zip", "tar.gz"]);
        // Second override: darwin with one format
        assert_eq!(overrides[1].os, "darwin");
        assert_eq!(
            overrides[1].formats.as_deref(),
            Some(&["tar.xz".to_string()][..])
        );
    } else {
        panic!("expected Configs variant");
    }
}

#[test]
fn test_config_parse_meta_builds_info_strip_allow() {
    use anodizer_core::config::Config;
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - meta: true
        strip_binary_directory: true
        allow_different_binary_count: true
        builds_info:
          owner: root
          group: root
          mode: 0o755
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
        assert_eq!(cfgs[0].meta, Some(true));
        assert_eq!(cfgs[0].strip_binary_directory, Some(true));
        assert_eq!(cfgs[0].allow_different_binary_count, Some(true));
        let bi = cfgs[0].builds_info.as_ref().unwrap();
        assert_eq!(bi.owner.as_deref(), Some("root"));
        assert_eq!(bi.group.as_deref(), Some("root"));
        assert_eq!(bi.mode, Some(anodizer_core::config::StringOrU32(0o755)));
    } else {
        panic!("expected Configs variant");
    }
}

#[test]
fn test_config_parse_archive_hooks() {
    use anodizer_core::config::Config;
    // Archive hooks use `before:` / `after:` (matching GoReleaser's archive pipe).
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - hooks:
          before:
            - echo pre-archive
          after:
            - echo post-archive
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
        let hooks = cfgs[0].hooks.as_ref().unwrap();
        let before = hooks.before.as_ref().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0], "echo pre-archive");
        let after = hooks.after.as_ref().unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], "echo post-archive");
    } else {
        panic!("expected Configs variant");
    }
}

// -----------------------------------------------------------------------
// gz format tests
// -----------------------------------------------------------------------

#[test]
fn test_create_gz() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content for gz").unwrap();

    let archive_path = tmp.path().join("mybin.gz");
    create_gz(&bin_path, &archive_path).unwrap();

    assert!(archive_path.exists());
    let len = fs::metadata(&archive_path).unwrap().len();
    assert!(len > 0, "gz archive should not be empty");

    // Verify we can decompress and get the original content
    let compressed = fs::read(&archive_path).unwrap();
    let mut dec = flate2::read::GzDecoder::new(&compressed[..]);
    let mut decompressed = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut decompressed).unwrap();
    assert_eq!(decompressed, b"binary content for gz");
}

#[test]
fn test_create_gz_nonexistent_fails() {
    let tmp = TempDir::new().unwrap();
    let archive_path = tmp.path().join("empty.gz");
    let nonexistent = tmp.path().join("does_not_exist");
    let result = create_gz(&nonexistent, &archive_path);
    assert!(result.is_err(), "gz with nonexistent file should fail");
}

// -----------------------------------------------------------------------
// Q17.1 — `xz` single-file format. Mirrors GoReleaser commit bb532b6
// (#6520, pkg/archive/xz/xz.go): a top-level xz container holds exactly
// one file. The unit-level writer + the stage-level dispatch both pin
// this contract.
// -----------------------------------------------------------------------

#[test]
fn test_create_xz_single_file_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content for xz").unwrap();

    let archive_path = tmp.path().join("mybin.xz");
    create_xz(&bin_path, &archive_path).unwrap();

    assert!(archive_path.exists());
    let len = fs::metadata(&archive_path).unwrap().len();
    assert!(len > 0, "xz archive should not be empty");

    // Decompress and verify the original content survives the round-trip.
    let compressed = fs::read(&archive_path).unwrap();
    let mut dec = xz2::read::XzDecoder::new(&compressed[..]);
    let mut decompressed = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut decompressed).unwrap();
    assert_eq!(decompressed, b"binary content for xz");
}

#[test]
fn test_create_xz_nonexistent_fails() {
    let tmp = TempDir::new().unwrap();
    let archive_path = tmp.path().join("empty.xz");
    let nonexistent = tmp.path().join("does_not_exist");
    let result = create_xz(&nonexistent, &archive_path);
    assert!(result.is_err(), "xz with nonexistent file should fail");
}

#[test]
fn test_archive_stage_xz_format_single_binary() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Crate dir is an isolated subdirectory with no LICENSE/README so
    // `resolve_default_extra_files` returns nothing and the binary is
    // the only file fed to the xz writer.
    let crate_dir = tmp.path().join("crate");
    fs::create_dir_all(&crate_dir).unwrap();
    let bin = crate_dir.join("myapp");
    fs::write(&bin, b"binary content").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: crate_dir.to_string_lossy().to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["xz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0].metadata.get("format"), Some(&"xz".to_string()));
    assert!(archives[0].path.exists(), "xz archive file should exist");
    assert!(
        archives[0].path.to_string_lossy().ends_with(".xz"),
        "xz archive should have .xz extension"
    );
    assert!(
        !archives[0].path.to_string_lossy().ends_with(".tar.xz"),
        "single-file xz must not be confused with the tar.xz container"
    );
}

#[test]
fn test_archive_stage_xz_format_multi_file_errors() {
    use anodizer_core::config::{
        ArchiveConfig, ArchiveFileSpec, ArchivesConfig, Config, CrateConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let crate_dir = tmp.path().join("crate");
    fs::create_dir_all(&crate_dir).unwrap();
    let bin = crate_dir.join("myapp");
    fs::write(&bin, b"binary content").unwrap();
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"MIT License").unwrap();
    let license_path = license.to_string_lossy().to_string();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: crate_dir.to_string_lossy().to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["xz".to_string()]),
            // Adding an extra file forces a multi-file payload, which xz
            // (a single-file format) must reject — mirrors the upstream
            // `xz: failed to add %s, only one file can be archived in xz`
            // error from `pkg/archive/xz/xz.go`.
            files: Some(vec![ArchiveFileSpec::Glob(license_path)]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    let result = ArchiveStage.run(&mut ctx);
    assert!(result.is_err(), "xz with multiple files must error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only one file can be archived in xz format"),
        "error must reference single-file xz contract, got: {err}"
    );
}

#[test]
fn test_archive_stage_gz_format() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary content").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["gz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0].metadata.get("format"), Some(&"gz".to_string()));
    assert!(archives[0].path.exists(), "gz archive file should exist");
    assert!(
        archives[0].path.to_string_lossy().ends_with(".gz"),
        "gz archive should have .gz extension"
    );
}

// -----------------------------------------------------------------------
// meta archive test
// -----------------------------------------------------------------------

#[test]
fn test_archive_stage_meta_no_binaries() {
    use anodizer_core::config::{
        ArchiveConfig, ArchiveFileSpec, ArchivesConfig, Config, CrateConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create extra files but no binary
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"MIT License").unwrap();
    let license_path = license.to_string_lossy().to_string();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            meta: Some(true),
            formats: Some(vec!["tar.gz".to_string()]),
            files: Some(vec![ArchiveFileSpec::Glob(license_path)]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    // No binary artifacts registered at all

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1, "meta archive should be created");
    assert_eq!(
        archives[0].metadata.get("meta"),
        Some(&"true".to_string()),
        "should be marked as meta"
    );
    assert!(archives[0].path.exists());

    // Verify the archive only contains the LICENSE file, no binaries
    let file = File::open(&archives[0].path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found = read_tar_entries(tar::Archive::new(dec));
    assert_eq!(
        found.len(),
        1,
        "meta archive should contain only the extra file"
    );
    assert!(found.contains_key("LICENSE"));
}

// -----------------------------------------------------------------------
// format_overrides.formats plural test
// -----------------------------------------------------------------------

#[test]
fn test_format_override_formats_plural() {
    // FormatOverride with plural formats should produce multiple formats
    let overrides = vec![FormatOverride {
        os: "windows".to_string(),
        formats: Some(vec!["zip".to_string(), "tar.gz".to_string()]),
    }];
    let result = formats_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides);
    assert_eq!(result, vec!["zip", "tar.gz"]);
}

#[test]
fn test_format_override_empty_formats_falls_back_to_default() {
    // Empty formats falls back to default_format
    let overrides = vec![FormatOverride {
        os: "windows".to_string(),
        formats: Some(vec![]),
    }];
    let result = formats_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides);
    assert_eq!(result, vec!["tar.xz"]);
}

#[test]
fn test_format_override_no_match_falls_back_to_default() {
    // No matching override falls back to default_format
    let overrides = vec![FormatOverride {
        os: "linux".to_string(),
        formats: Some(vec!["tar.gz".to_string()]),
    }];
    let result = formats_for_target("x86_64-pc-windows-msvc", "zip", &overrides);
    assert_eq!(result, vec!["zip"]);
}

/// Pins C-new-5: FormatOverride.os matches via prefix, mirroring GR's
/// `strings.HasPrefix(platform, override.Goos)`. For canonical OS names
/// the behavior is identical to `==`; the prefix relaxation kicks in for
/// any future os value that gains a sub-variant suffix.
#[test]
fn test_format_override_prefix_match() {
    let overrides = vec![FormatOverride {
        os: "lin".to_string(), // prefix of "linux"
        formats: Some(vec!["tar.gz".to_string()]),
    }];
    let result = formats_for_target("x86_64-unknown-linux-gnu", "zip", &overrides);
    assert_eq!(
        result,
        vec!["tar.gz"],
        "prefix-matching FormatOverride must apply"
    );
}

/// Negative pin for C-new-5: a prefix that does NOT match the target's os
/// field falls through to the default (e.g., `os: "darw"` against linux).
#[test]
fn test_format_override_prefix_no_match() {
    let overrides = vec![FormatOverride {
        os: "darw".to_string(),
        formats: Some(vec!["tar.gz".to_string()]),
    }];
    let result = formats_for_target("x86_64-unknown-linux-gnu", "zip", &overrides);
    assert_eq!(
        result,
        vec!["zip"],
        "prefix mismatch must fall through to default"
    );
}

/// Pins W2: empty `FormatOverride.os` is rejected as a typo guard. A user
/// who accidentally writes `os:` (yaml-empty) gets a clean fallback to the
/// default format instead of an empty-prefix match-everything override.
/// Anodizer-stricter than GR.
#[test]
fn test_format_override_empty_os_rejected() {
    let overrides = vec![FormatOverride {
        os: String::new(),
        formats: Some(vec!["tar.gz".to_string()]),
    }];
    let result = formats_for_target("x86_64-unknown-linux-gnu", "zip", &overrides);
    assert_eq!(
        result,
        vec!["zip"],
        "empty os must NOT match every target; falls through to default"
    );
}

// -----------------------------------------------------------------------
// allow_different_binary_count test (warning only)
// -----------------------------------------------------------------------

#[test]
fn test_allow_different_binary_count_default_errors_on_mismatch() {
    // GoReleaser errors (not warns) when binary counts differ and
    // allow_different_binary_count is false (default).
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let linux_bin = tmp.path().join("myapp-linux");
    let win_bin1 = tmp.path().join("myapp-win1");
    let win_bin2 = tmp.path().join("myapp-win2");
    fs::write(&linux_bin, b"linux binary").unwrap();
    fs::write(&win_bin1, b"windows binary 1").unwrap();
    fs::write(&win_bin2, b"windows binary 2").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                // allow_different_binary_count is None (default false) - should error
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    // Different binary counts per target
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: linux_bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: win_bin1,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: win_bin2,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "helper".to_string())]),
        size: None,
    });

    // Should error when binary counts differ (matching GoReleaser behavior)
    let result = ArchiveStage.run(&mut ctx);
    assert!(
        result.is_err(),
        "different binary counts should error, not warn"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("binary counts differ"),
        "error should mention binary count mismatch, got: {err}"
    );
    assert!(
        err.contains("allow_different_binary_count"),
        "error should suggest the fix, got: {err}"
    );
}

/// Regression test for parity with GoReleaser archive/archive.go:129 —
/// plural `formats: ["binary"]` must exempt allow_different_binary_count
/// the same way singular `format: "binary"` does.
#[test]
fn test_binary_format_plural_exempts_different_binary_count_check() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let linux_bin = tmp.path().join("myapp-linux");
    let win_bin1 = tmp.path().join("myapp-win1");
    let win_bin2 = tmp.path().join("myapp-win2");
    fs::write(&linux_bin, b"linux binary").unwrap();
    fs::write(&win_bin1, b"windows binary 1").unwrap();
    fs::write(&win_bin2, b"windows binary 2").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                // plural-only `formats` — the old bug would miss this
                // because the exemption checked only `format`.
                formats: Some(vec!["binary".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: linux_bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: win_bin1,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: win_bin2,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "helper".to_string())]),
        size: None,
    });

    let result = ArchiveStage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "plural `formats: [binary]` must exempt the mismatched-count \
             check (got error: {:?})",
        result.err()
    );
}

// -----------------------------------------------------------------------
// strip_binary_directory metadata test
// -----------------------------------------------------------------------

#[test]
fn test_strip_binary_directory_metadata() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                strip_binary_directory: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(
        archives[0].metadata.get("strip_binary_directory"),
        Some(&"true".to_string()),
    );
}

// -----------------------------------------------------------------------
// resolve_file_specs tests
// -----------------------------------------------------------------------

#[test]
fn test_resolve_file_specs_glob() {
    use anodizer_core::config::ArchiveFileSpec;

    let tmp = TempDir::new().unwrap();
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"MIT").unwrap();

    let specs = vec![ArchiveFileSpec::Glob(license.to_string_lossy().to_string())];
    let resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].src, license);
    assert!(resolved[0].dst.is_none());
    assert!(resolved[0].info.is_none());
}

#[test]
fn test_resolve_file_specs_detailed() {
    use anodizer_core::config::{ArchiveFileInfo, ArchiveFileSpec};

    let tmp = TempDir::new().unwrap();
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"MIT").unwrap();

    let specs = vec![ArchiveFileSpec::Detailed {
        src: license.to_string_lossy().to_string(),
        dst: Some("licenses/".to_string()),
        info: Some(ArchiveFileInfo {
            owner: Some("root".to_string()),
            group: Some("root".to_string()),
            mode: Some(anodizer_core::config::StringOrU32(0o644)),
            mtime: None,
        }),
        strip_parent: None,
    }];
    let resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].src, license);
    // With LCP logic: single file, LCP is the file path itself, so
    // prefix_dir = parent dir, rel = filename, dst = "licenses/LICENSE"
    assert_eq!(resolved[0].dst.as_deref(), Some("licenses/LICENSE"));
    let info = resolved[0].info.as_ref().unwrap();
    assert_eq!(info.owner.as_deref(), Some("root"));
    assert_eq!(info.mode, Some(anodizer_core::config::StringOrU32(0o644)));
}

// -----------------------------------------------------------------------
// longest_common_prefix tests
// -----------------------------------------------------------------------

#[test]
fn test_lcp_empty() {
    assert_eq!(crate::file_specs::longest_common_prefix(&[]), "");
}

#[test]
fn test_lcp_single() {
    let strs = vec!["/home/user/docs/README.md".to_string()];
    assert_eq!(
        crate::file_specs::longest_common_prefix(&strs),
        "/home/user/docs/README.md"
    );
}

#[test]
fn test_lcp_multiple_common() {
    let strs = vec![
        "/home/user/docs/README.md".to_string(),
        "/home/user/docs/guide/intro.md".to_string(),
        "/home/user/docs/guide/advanced.md".to_string(),
    ];
    assert_eq!(
        crate::file_specs::longest_common_prefix(&strs),
        "/home/user/docs/"
    );
}

#[test]
fn test_lcp_no_common_prefix() {
    let strs = vec![
        "/usr/local/bin/foo".to_string(),
        "/home/user/bar".to_string(),
    ];
    assert_eq!(crate::file_specs::longest_common_prefix(&strs), "/");
}

#[test]
fn test_lcp_identical_strings() {
    let strs = vec![
        "/home/user/file.txt".to_string(),
        "/home/user/file.txt".to_string(),
    ];
    assert_eq!(
        crate::file_specs::longest_common_prefix(&strs),
        "/home/user/file.txt"
    );
}

// -----------------------------------------------------------------------
// resolve_file_specs with dst — directory preservation via LCP
// -----------------------------------------------------------------------

#[test]
fn test_resolve_file_specs_dst_preserves_directory_structure() {
    use anodizer_core::config::ArchiveFileSpec;

    let tmp = TempDir::new().unwrap();
    let docs_dir = tmp.path().join("docs");
    let guide_dir = docs_dir.join("guide");
    fs::create_dir_all(&guide_dir).unwrap();
    fs::write(docs_dir.join("README.md"), b"readme").unwrap();
    fs::write(guide_dir.join("intro.md"), b"intro").unwrap();

    let glob_pattern = format!("{}/**/*.md", docs_dir.display());
    let specs = vec![ArchiveFileSpec::Detailed {
        src: glob_pattern,
        dst: Some("mydocs".to_string()),
        info: None,
        strip_parent: None,
    }];

    let mut resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 2);

    // Sort by dst for deterministic assertions
    resolved.sort_by(|a, b| a.dst.cmp(&b.dst));

    // The LCP of "/tmp/.../docs/README.md" and "/tmp/.../docs/guide/intro.md"
    // is "/tmp/.../docs/" which IS an existing directory, so prefix_dir = docs_dir.
    // Relative paths: "README.md" and "guide/intro.md"
    // Destinations: "mydocs/README.md" and "mydocs/guide/intro.md"
    assert_eq!(resolved[0].dst.as_deref(), Some("mydocs/README.md"));
    assert_eq!(resolved[1].dst.as_deref(), Some("mydocs/guide/intro.md"));
}

#[test]
fn test_resolve_file_specs_dst_with_strip_parent_ignores_lcp() {
    use anodizer_core::config::ArchiveFileSpec;

    let tmp = TempDir::new().unwrap();
    let docs_dir = tmp.path().join("docs");
    let guide_dir = docs_dir.join("guide");
    fs::create_dir_all(&guide_dir).unwrap();
    fs::write(docs_dir.join("README.md"), b"readme").unwrap();
    fs::write(guide_dir.join("intro.md"), b"intro").unwrap();

    let glob_pattern = format!("{}/**/*.md", docs_dir.display());
    let specs = vec![ArchiveFileSpec::Detailed {
        src: glob_pattern,
        dst: Some("mydocs".to_string()),
        info: None,
        strip_parent: Some(true),
    }];

    let resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 2);

    // GoReleaser archivefiles.go:117-118 — with both dst AND strip_parent,
    // per-file dst becomes dst/basename(path). Each file gets its own
    // basename appended so they do not collide at a single dst.
    let dst_values: std::collections::HashSet<Option<String>> =
        resolved.iter().map(|r| r.dst.clone()).collect();
    assert!(dst_values.contains(&Some("mydocs/README.md".to_string())));
    assert!(dst_values.contains(&Some("mydocs/intro.md".to_string())));
    // strip_parent is collapsed into the computed dst, so the flag on the
    // resolved entry is false (caller has no more work to do).
    for r in &resolved {
        assert!(!r.strip_parent);
    }
}

#[test]
fn test_resolve_file_specs_literal_src_with_dst_preserves_filename() {
    use anodizer_core::config::ArchiveFileSpec;

    let tmp = TempDir::new().unwrap();
    let license = tmp.path().join("LICENSE");
    fs::write(&license, b"MIT License").unwrap();

    // Literal (non-glob) src with a dst directory — our LCP logic should
    // produce "licenses/LICENSE" rather than renaming the file to "licenses".
    // This is an intentional divergence from GoReleaser, which would rename
    // the file.
    let specs = vec![ArchiveFileSpec::Detailed {
        src: license.to_string_lossy().to_string(),
        dst: Some("licenses".to_string()),
        info: None,
        strip_parent: None,
    }];

    let resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].src, license);
    assert_eq!(resolved[0].dst.as_deref(), Some("licenses/LICENSE"));
}

#[test]
fn test_resolve_file_specs_dst_partial_filename_lcp_fallback() {
    use anodizer_core::config::ArchiveFileSpec;

    let tmp = TempDir::new().unwrap();
    // Two files whose names share a prefix — the LCP of their full paths
    // will be something like "/tmp/.../file_" which is NOT a directory.
    // The code should fall back to the parent directory so both files
    // appear under dst with just their filenames.
    let alpha = tmp.path().join("file_alpha.txt");
    let beta = tmp.path().join("file_beta.txt");
    fs::write(&alpha, b"alpha").unwrap();
    fs::write(&beta, b"beta").unwrap();

    let glob_pattern = format!("{}/file_*.txt", tmp.path().display());
    let specs = vec![ArchiveFileSpec::Detailed {
        src: glob_pattern,
        dst: Some("output".to_string()),
        info: None,
        strip_parent: None,
    }];

    let mut resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 2);

    // Sort for deterministic assertions
    resolved.sort_by(|a, b| a.dst.cmp(&b.dst));

    // LCP is "/tmp/.../file_" which is not a directory, so prefix_dir
    // falls back to the parent dir ("/tmp/.../"). Relative paths are
    // "file_alpha.txt" and "file_beta.txt".
    assert_eq!(resolved[0].dst.as_deref(), Some("output/file_alpha.txt"));
    assert_eq!(resolved[1].dst.as_deref(), Some("output/file_beta.txt"));
}

// -----------------------------------------------------------------------
// builds_info: verify permissions apply to tar entries
// -----------------------------------------------------------------------

#[test]
fn test_append_tar_entry_with_file_info_mode() {
    use anodizer_core::config::ArchiveFileInfo;

    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("test.tar");
    let out_file = File::create(&archive_path).unwrap();
    let mut tar = tar::Builder::new(out_file);

    let info = ArchiveFileInfo {
        mode: Some(anodizer_core::config::StringOrU32(0o755)),
        owner: Some("deploy".to_string()),
        group: Some("staff".to_string()),
        mtime: None,
    };

    formats::append_tar_entry(&mut tar, &bin_path, Path::new("mybin"), None, Some(&info)).unwrap();
    tar.finish().unwrap();

    // Read back the archive and verify permissions
    let file = File::open(&archive_path).unwrap();
    let mut tar = tar::Archive::new(file);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    let header = entry.header();

    // Mode should be 0o755
    assert_eq!(header.mode().unwrap() & 0o777, 0o755, "mode should be 0755");
    assert_eq!(
        header.username().unwrap().unwrap(),
        "deploy",
        "owner should be 'deploy'"
    );
    assert_eq!(
        header.groupname().unwrap().unwrap(),
        "staff",
        "group should be 'staff'"
    );
}

#[test]
fn test_write_tar_entries_with_file_info() {
    use anodizer_core::config::ArchiveFileInfo;

    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("mybin");
    fs::write(&bin_path, b"binary content").unwrap();

    let archive_path = tmp.path().join("test.tar");
    let out_file = File::create(&archive_path).unwrap();
    let mut tar = tar::Builder::new(out_file);

    let info = ArchiveFileInfo {
        mode: Some(anodizer_core::config::StringOrU32(0o755)),
        owner: None,
        group: None,
        mtime: None,
    };

    formats::write_tar_entries(
        &mut tar,
        &[bin_path.as_path()],
        None,
        None,
        None,
        Some(&info),
        "test",
    )
    .unwrap();
    tar.finish().unwrap();

    // Read back and verify
    let file = File::open(&archive_path).unwrap();
    let mut tar = tar::Archive::new(file);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    assert_eq!(
        entry.header().mode().unwrap() & 0o777,
        0o755,
        "write_tar_entries should apply file_info mode"
    );
}

// ---------------------------------------------------------------------------
// deduplicate_entries
// ---------------------------------------------------------------------------

#[test]
fn test_deduplicate_entries_keeps_first_skips_later() {
    let entries = vec![
        ArchiveEntry {
            src: PathBuf::from("/src/a/mybin"),
            archive_name: PathBuf::from("bin/mybin"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("/src/b/mybin"),
            archive_name: PathBuf::from("bin/mybin"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("LICENSE"),
            archive_name: PathBuf::from("LICENSE"),
            info: None,
        },
    ];

    let deduped = deduplicate_entries(entries);
    assert_eq!(deduped.len(), 2, "duplicate should be removed");
    assert_eq!(deduped[0].src, PathBuf::from("/src/a/mybin"));
    assert_eq!(deduped[0].archive_name, PathBuf::from("bin/mybin"));
    assert_eq!(deduped[1].archive_name, PathBuf::from("LICENSE"));
}

// ---------------------------------------------------------------------------
// sort_entries
// ---------------------------------------------------------------------------

#[test]
fn test_sort_entries_by_archive_name() {
    let entries = vec![
        ArchiveEntry {
            src: PathBuf::from("z.txt"),
            archive_name: PathBuf::from("c.txt"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("a.txt"),
            archive_name: PathBuf::from("a.txt"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("m.txt"),
            archive_name: PathBuf::from("b.txt"),
            info: None,
        },
    ];

    let sorted = sort_entries(entries);
    let names: Vec<String> = sorted
        .iter()
        .map(|e| e.archive_name.to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
}

// ---------------------------------------------------------------------------
// render_file_info
// ---------------------------------------------------------------------------

#[test]
fn test_render_file_info_templates() {
    use anodizer_core::config::ArchiveFileInfo;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("ProjectName", "myapp");

    let info = ArchiveFileInfo {
        owner: Some("{{ .ProjectName }}".to_string()),
        group: Some("staff".to_string()),
        mode: Some(anodizer_core::config::StringOrU32(0o755)),
        mtime: Some("{{ .Version }}".to_string()),
    };

    let rendered = render_file_info(&info, &ctx).unwrap();
    assert_eq!(rendered.owner.as_deref(), Some("myapp"));
    assert_eq!(rendered.group.as_deref(), Some("staff"));
    assert_eq!(
        rendered.mode,
        Some(anodizer_core::config::StringOrU32(0o755))
    ); // mode not rendered
    assert_eq!(rendered.mtime.as_deref(), Some("1.2.3"));
}

#[test]
fn test_archive_stage_binaries_filter() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_a = tmp.path().join("app-a");
    let bin_b = tmp.path().join("app-b");
    fs::write(&bin_a, b"binary-a").unwrap();
    fs::write(&bin_b, b"binary-b").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some("filtered-archive".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            binaries: Some(vec!["app-a".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    for (name, path) in [("app-a", &bin_a), ("app-b", &bin_b)] {
        let mut metadata = HashMap::new();
        metadata.insert("binary".to_string(), name.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);

    // Open archive and verify only app-a is inside, not app-b
    let archive_path = &archives[0].path;
    let file = File::open(archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let found_files = read_tar_entries(tar::Archive::new(dec));

    assert!(
        found_files.keys().any(|n| n.contains("app-a")),
        "should contain app-a: {:?}",
        found_files.keys().collect::<Vec<_>>()
    );
    assert!(
        !found_files.keys().any(|n| n.contains("app-b")),
        "should NOT contain app-b: {:?}",
        found_files.keys().collect::<Vec<_>>()
    );
}

#[test]
fn test_default_name_template_includes_amd64_suffix() {
    let tmpl = default_name_template();
    assert!(
        tmpl.contains("Amd64"),
        "default name template should contain Amd64 conditional: {tmpl}"
    );
    let bin_tmpl = default_binary_name_template();
    assert!(
        bin_tmpl.contains("Amd64"),
        "default binary name template should contain Amd64 conditional: {bin_tmpl}"
    );
}

#[test]
fn test_default_template_renders_amd64_v2_suffix() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Amd64", "v2");

    let result = ctx.render_template(default_name_template()).unwrap();
    assert_eq!(result, "myapp_1.0.0_linux_amd64v2");
}

#[test]
fn test_default_template_omits_amd64_v1_suffix() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Amd64", "v1");

    let result = ctx.render_template(default_name_template()).unwrap();
    assert_eq!(result, "myapp_1.0.0_linux_amd64");
}

// --- Windows binary-format archives get .exe suffix ---

#[test]
fn test_archive_binary_format_windows_appends_exe() {
    // Windows binaries keep their
    // executable suffix even when packaged as `format: binary`.
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp.exe");
    fs::write(&bin_path, b"raw windows binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["binary".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let bins = ctx.artifacts.by_kind(ArtifactKind::UploadableBinary);
    assert_eq!(bins.len(), 1);
    let name = bins[0].path.file_name().unwrap().to_string_lossy();
    assert!(
        name.ends_with(".exe"),
        "Windows binary-format upload must end with .exe, got: {name}"
    );
    assert!(bins[0].path.exists());
}

#[test]
fn test_archive_meta_empty_files_hard_errors() {
    // meta archive with zero files must
    // hard-error rather than silently emit an empty archive. Previously was
    // a silent failure mode that masked a real config bug.
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some("{{ .ProjectName }}-meta".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            meta: Some(true),
            // No files configured — auto-include is also disabled for meta.
            files: Some(vec![]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let err = ArchiveStage
        .run(&mut ctx)
        .expect_err("meta archive with zero files must bail");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("meta archive") && msg.contains("zero files"),
        "error should name the meta/zero-files condition, got: {msg}"
    );
}

#[test]
fn test_archive_binary_format_linux_keeps_no_extension() {
    // Regression guard: Linux/macOS binary-format archives should NOT get .exe.
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"raw linux binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some(
                "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
            ),
            formats: Some(vec!["binary".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });
    ArchiveStage.run(&mut ctx).unwrap();
    let bins = ctx.artifacts.by_kind(ArtifactKind::UploadableBinary);
    assert_eq!(bins.len(), 1);
    let name = bins[0].path.file_name().unwrap().to_string_lossy();
    assert!(
        !name.ends_with(".exe"),
        "Linux binary-format upload should not get .exe, got: {name}"
    );
}

/// regression: in a multi-crate config with no explicit
/// `archive.name_template:`, every crate's archive must resolve to a
/// distinct filename. Pre-fix the canonical default
/// `{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}` produced
/// the same stem for every crate and caused the dry-run to emit
/// `Warning: artifact '<name>' already registered` once per crate.
#[test]
fn test_archive_multi_crate_default_template_uses_crate_name() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_a = tmp.path().join("crate-a");
    fs::write(&bin_a, b"binary-a").unwrap();
    let bin_b = tmp.path().join("crate-b");
    fs::write(&bin_b, b"binary-b").unwrap();

    let mk_crate = |name: &str| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "anodizer".to_string();
    config.dist = dist;
    config.crates = vec![mk_crate("crate-a"), mk_crate("crate-b")];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let mk_artifact = |bin: PathBuf, crate_name: &str, bin_name: &str| Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: crate_name.to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), bin_name.to_string());
            m
        },
        size: None,
    };
    ctx.artifacts.add(mk_artifact(bin_a, "crate-a", "crate-a"));
    ctx.artifacts.add(mk_artifact(bin_b, "crate-b", "crate-b"));

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 2, "two crates → two archives");
    let names: Vec<String> = archives
        .iter()
        .map(|a| a.metadata.get("name").cloned().unwrap_or_default())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("crate-a_")),
        "crate-a archive must use CrateName: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("crate-b_")),
        "crate-b archive must use CrateName: {names:?}"
    );
    assert_ne!(
        names[0], names[1],
        "multi-crate archives must have distinct stems: {names:?}"
    );
}

/// a single-crate config keeps the GoReleaser-canonical
/// `{{ .ProjectName }}_..` default — the multi-crate template change
/// is opt-in via crate count, not unconditional.
#[test]
fn test_archive_single_crate_keeps_project_name_default() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin = tmp.path().join("myapp");
    fs::write(&bin, b"binary").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp-project".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    let name = archives[0]
        .metadata
        .get("name")
        .cloned()
        .unwrap_or_default();
    assert!(
        name.starts_with("myapp-project_"),
        "single-crate config must keep ProjectName-keyed default: {name}"
    );
}

// ---------------------------------------------------------------------------
// 2026-05-08 second-opinion parity audit regressions (C1, C2, Q-arch1)
// ---------------------------------------------------------------------------

/// C1 — Build stage writes the canonical GR `DynamicallyLinked` extra key
/// (mirrors `artifact.ExtranDynLink`). The archive stage previously read a
/// snake-case `dynamically_linked`, so the propagation never fired and the
/// resulting archive's `ndynlink` flag was always absent. Confirm the
/// camel-case key is honored end-to-end.
#[test]
fn test_archive_dynlink_propagation_uses_canonical_key() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    let mut meta = HashMap::new();
    meta.insert("binary".to_string(), "myapp".to_string());
    // GoReleaser-canonical key — see artifact.ExtranDynLink.
    meta.insert("DynamicallyLinked".to_string(), "true".to_string());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: meta,
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(
        archives[0].metadata.get("ndynlink").map(String::as_str),
        Some("true"),
        "archive must mark `ndynlink` when source binary carries the canonical \
         `DynamicallyLinked` GR extra"
    );
}

/// C2 — Archive metadata previously dropped `amd64_variant` so publishers
/// (winget/scoop/aur/krew) that filter on it would silently match v2/v3/v4
/// binaries as v1. Mirrors GR archive.go:255 (`art.Goamd64 = binaries[0].Goamd64`).
#[test]
fn test_archive_amd64_variant_propagated_from_first_binary() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    let mut meta = HashMap::new();
    meta.insert("binary".to_string(), "myapp".to_string());
    meta.insert("amd64_variant".to_string(), "v3".to_string());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: meta,
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);
    assert_eq!(
        archives[0]
            .metadata
            .get("amd64_variant")
            .map(String::as_str),
        Some("v3"),
        "archive metadata must copy `amd64_variant` from the first source binary \
         so publisher filters resolve the correct microarch variant"
    );
}

/// Q-arch1 — Multi-crate archives must render `{{ .ProjectName }}` to the
/// per-crate name (with `{{ .CrateName }}` still available separately) so
/// users migrating GR configs whose name templates reference `ProjectName`
/// see GR-equivalent filenames. The `default_name_template_multi_crate()`
/// also resolves to the canonical GR shape after the iteration override.
#[test]
fn test_multi_crate_archive_projectname_resolves_to_crate() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_a = tmp.path().join("crate-a");
    let bin_b = tmp.path().join("crate-b");
    fs::write(&bin_a, b"a").unwrap();
    fs::write(&bin_b, b"b").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("workspace")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![
            CrateConfig {
                name: "crate-a".to_string(),
                path: "crate-a".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    name_template: Some(
                        "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                    ),
                    formats: Some(vec!["tar.gz".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            CrateConfig {
                name: "crate-b".to_string(),
                path: "crate-b".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    name_template: Some(
                        "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                    ),
                    formats: Some(vec!["tar.gz".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_a,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "crate-a".to_string(),
        metadata: HashMap::from([("binary".to_string(), "crate-a".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_b,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "crate-b".to_string(),
        metadata: HashMap::from([("binary".to_string(), "crate-b".to_string())]),
        size: None,
    });

    ArchiveStage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 2);
    let names: Vec<String> = archives
        .iter()
        .filter_map(|a| a.metadata.get("name").cloned())
        .collect();
    assert!(
        names.iter().any(|n| n == "crate-a-1.0.0-linux-amd64"),
        "expected crate-a archive stem in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "crate-b-1.0.0-linux-amd64"),
        "expected crate-b archive stem in {names:?}"
    );
    // Global ProjectName template var must be restored after the multi-crate
    // iteration so downstream stages still see the workspace project name.
    assert_eq!(
        ctx.template_vars().get("ProjectName").map(String::as_str),
        Some("workspace"),
    );
}

// ---------------------------------------------------------------------------
// SOURCE_DATE_EPOCH byte-stability regression
// ---------------------------------------------------------------------------
//
// stage-archive's tar writer reads `mtime` from the SDE-derived value
// resolved in `run.rs` (CommitTimestamp template var or SOURCE_DATE_EPOCH
// env override). The audit found no `Utc::now()` / `SystemTime::now()`
// callsites in stage-archive, so this test only pins the contract that
// `create_tar` with the same `mtime` produces byte-identical output
// across calls. A future refactor that introduces a clock read inside
// `write_tar_entries` (e.g. defaulting an mtime when None) regresses
// this test.

#[test]
fn archive_byte_stable_for_same_sde() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("hello.txt");
    fs::write(&src, b"hello world").unwrap();

    let out_a = tmp.path().join("a.tar");
    let out_b = tmp.path().join("b.tar");

    // Same SDE-derived mtime for both runs. Pinned value: SDE 1_715_000_000.
    let mtime = Some(1_715_000_000u64);

    formats::create_tar(
        &[src.as_path()],
        &out_a,
        Some(tmp.path()),
        None,
        mtime,
        None,
    )
    .expect("create_tar a");
    formats::create_tar(
        &[src.as_path()],
        &out_b,
        Some(tmp.path()),
        None,
        mtime,
        None,
    )
    .expect("create_tar b");

    let bytes_a = fs::read(&out_a).unwrap();
    let bytes_b = fs::read(&out_b).unwrap();
    assert_eq!(
        bytes_a, bytes_b,
        "tar archives built with the same SDE-derived mtime must be byte-identical"
    );
}

#[test]
fn archive_byte_differs_for_different_sde() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("hello.txt");
    fs::write(&src, b"hello world").unwrap();

    let out_a = tmp.path().join("a.tar");
    let out_b = tmp.path().join("b.tar");

    formats::create_tar(
        &[src.as_path()],
        &out_a,
        Some(tmp.path()),
        None,
        Some(1_715_000_000u64),
        None,
    )
    .expect("create_tar a");
    formats::create_tar(
        &[src.as_path()],
        &out_b,
        Some(tmp.path()),
        None,
        Some(1_716_000_000u64),
        None,
    )
    .expect("create_tar b");

    let bytes_a = fs::read(&out_a).unwrap();
    let bytes_b = fs::read(&out_b).unwrap();
    assert_ne!(
        bytes_a, bytes_b,
        "different mtimes must produce different tar bytes (sanity check on SDE wiring)"
    );
}

// ---------------------------------------------------------------------------
// archive before/after hooks
// ---------------------------------------------------------------------------

/// Drive ArchiveStage with a configured `hooks.before` + `hooks.after`
/// pair that touches sentinel files, then return the sentinel paths so
/// the test can assert which hooks fired.
fn run_with_hook_sentinels(
    crate_dir: &Path,
    dist: PathBuf,
    formats: Vec<String>,
    before_sentinel: &Path,
    after_sentinel: &Path,
) -> anyhow::Result<()> {
    use anodizer_core::config::{
        ArchiveConfig, ArchiveHooksConfig, ArchivesConfig, CrateConfig, HookEntry,
    };
    use anodizer_core::test_helpers::TestContextBuilder;

    let bin_path = crate_dir.join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    // Use templated paths so {{ Format }} appears in the touch target —
    // that's how we verify per-format firing.
    let before_cmd = format!(
        "touch {}.{{{{ Format }}}}",
        before_sentinel.to_string_lossy()
    );
    let after_cmd = format!(
        "touch {}.{{{{ Format }}}}",
        after_sentinel.to_string_lossy()
    );

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                id: Some("default".to_string()),
                name_template: Some("{{ ProjectName }}-{{ Version }}".to_string()),
                formats: Some(formats),
                hooks: Some(ArchiveHooksConfig {
                    before: Some(vec![HookEntry::Simple(before_cmd)]),
                    after: Some(vec![HookEntry::Simple(after_cmd)]),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx)
}

#[test]
fn archive_hooks_skipped_for_binary_format() {
    let tmp = TempDir::new().unwrap();
    let before = tmp.path().join("BEFORE");
    let after = tmp.path().join("AFTER");

    run_with_hook_sentinels(
        tmp.path(),
        tmp.path().join("dist"),
        vec!["binary".to_string()],
        &before,
        &after,
    )
    .expect("archive stage runs for binary format");

    // GR contract: hooks skipped when format is `binary`. The sentinels
    // would be `BEFORE.binary` / `AFTER.binary` if the hooks had fired.
    let before_marker = tmp.path().join("BEFORE.binary");
    let after_marker = tmp.path().join("AFTER.binary");
    assert!(
        !before_marker.exists(),
        "before hook MUST NOT fire for format: binary"
    );
    assert!(
        !after_marker.exists(),
        "after hook MUST NOT fire for format: binary"
    );
}

#[test]
fn archive_hooks_run_per_format() {
    let tmp = TempDir::new().unwrap();
    let before = tmp.path().join("BEFORE");
    let after = tmp.path().join("AFTER");

    run_with_hook_sentinels(
        tmp.path(),
        tmp.path().join("dist"),
        vec!["tar.gz".to_string(), "zip".to_string()],
        &before,
        &after,
    )
    .expect("archive stage runs for multi-format");

    // GR contract: "If multiple formats are set, hooks will be executed
    // for each format" (`archives.md:183`). Each format must observe
    // its own `.Format` value so the per-format `touch X.{{ Format }}`
    // creates one sentinel per format.
    let before_tar = tmp.path().join("BEFORE.tar.gz");
    let before_zip = tmp.path().join("BEFORE.zip");
    let after_tar = tmp.path().join("AFTER.tar.gz");
    let after_zip = tmp.path().join("AFTER.zip");
    assert!(
        before_tar.exists(),
        "before hook must fire for tar.gz format"
    );
    assert!(before_zip.exists(), "before hook must fire for zip format");
    assert!(after_tar.exists(), "after hook must fire for tar.gz format");
    assert!(after_zip.exists(), "after hook must fire for zip format");
}

#[test]
fn archive_after_hook_sees_artifact_path() {
    use anodizer_core::config::{
        ArchiveConfig, ArchiveHooksConfig, ArchivesConfig, CrateConfig, HookEntry,
    };
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    let sentinel = tmp.path().join("SAW_ARTIFACT");
    // `.ArtifactPath` must be resolved to a non-empty value for the
    // after-hook so users can post-process the just-written archive.
    let after_cmd = format!(
        "test -n '{{{{ ArtifactPath }}}}' && touch {}",
        sentinel.to_string_lossy()
    );

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                id: Some("default".to_string()),
                name_template: Some("{{ ProjectName }}-{{ Version }}".to_string()),
                formats: Some(vec!["tar.gz".to_string()]),
                hooks: Some(ArchiveHooksConfig {
                    before: None,
                    after: Some(vec![HookEntry::Simple(after_cmd)]),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).expect("archive stage runs");

    assert!(
        sentinel.exists(),
        "after-hook must see `.ArtifactPath` populated"
    );
}

/// `archives[].templated_files` must render once per (target, format)
/// pair, with `.Format` resolving to the current archive's format string
/// in the rendered output path.
///
/// Setup: one crate with one binary, archives configured with
/// `formats: [tar.gz, zip]` and a single `templated_files` entry whose
/// `dst` embeds `{{ .Format }}`. After running the stage, two archives
/// are produced (one per format) and each must contain the templated
/// file rendered with its own `.Format` value.
#[test]
fn test_archive_templated_files_per_format() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig, TemplateFileConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Stand up the source template file referenced by templated_files.src.
    // It renders to a body that embeds .Format so we can also assert
    // contents (not just the dst path).
    let tpl_path = tmp.path().join("formats.tpl");
    fs::write(&tpl_path, "format={{ .Format }}\n").unwrap();

    let bin_path = tmp.path().join("myapp");
    fs::write(&bin_path, b"fake binary").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                formats: Some(vec!["tar.gz".to_string(), "zip".to_string()]),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                templated_files: Some(vec![TemplateFileConfig {
                    id: Some("formats".to_string()),
                    src: tpl_path.to_string_lossy().to_string(),
                    // .Format embedded in dst — must resolve per archive.
                    dst: "{{ .Format }}/info.txt".to_string(),
                    mode: None,
                    skip: None,
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: bin_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("binary".to_string(), "myapp".to_string());
            m
        },
        size: None,
    });

    let stage = ArchiveStage;
    stage.run(&mut ctx).expect("archive stage runs");

    // Two archives: one .tar.gz, one .zip.
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 2, "expected one archive per format");

    let tar_archive = archives
        .iter()
        .find(|a| a.path.to_string_lossy().ends_with(".tar.gz"))
        .expect(".tar.gz archive must exist");
    let zip_archive = archives
        .iter()
        .find(|a| a.path.to_string_lossy().ends_with(".zip"))
        .expect(".zip archive must exist");

    // .tar.gz: read entries, look for `tar.gz/info.txt`.
    let file = File::open(&tar_archive.path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let tar_entries = read_tar_entries(tar::Archive::new(dec));
    let tar_dst = "tar.gz/info.txt";
    assert!(
        tar_entries.contains_key(tar_dst),
        ".tar.gz archive must contain `{tar_dst}`; got entries: {:?}",
        tar_entries.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        tar_entries[tar_dst],
        b"format=tar.gz\n".to_vec(),
        ".Format must resolve to 'tar.gz' inside the rendered file body"
    );

    // .zip: enumerate entries, look for `zip/info.txt`.
    let file = File::open(&zip_archive.path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let zip_dst = "zip/info.txt";
    let mut zip_body = Vec::new();
    let mut found_zip = false;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        if entry.name() == zip_dst {
            std::io::Read::read_to_end(&mut entry, &mut zip_body).unwrap();
            found_zip = true;
            break;
        }
    }
    assert!(
        found_zip,
        ".zip archive must contain `{zip_dst}` — .Format must resolve to 'zip'"
    );
    assert_eq!(
        zip_body,
        b"format=zip\n".to_vec(),
        ".Format must resolve to 'zip' inside the rendered file body"
    );
}
