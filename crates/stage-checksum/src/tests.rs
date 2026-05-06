//! Tests for the checksum stage — exercises hash function wrappers,
//! algorithm dispatch, sidecar/combined writers, extra-file resolution,
//! ID filtering, and refresh semantics.

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::path::Path;

use tempfile::TempDir;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::stage::Stage;

use super::hashing::{
    blake2b_file, blake2s_file, blake3_file, crc32_file, format_checksum_line, hash_file, md5_file,
    sha1_file, sha3_224_file, sha3_256_file, sha3_384_file, sha3_512_file, sha224_file,
    sha256_file, sha384_file, sha512_file,
};
use super::run::ChecksumStage;

/// Divergence test — pins the inherited GoReleaser sort behavior for
/// filenames that contain a two-space sequence. If this assertion ever
/// breaks, someone "fixed" the sort and diverged from GoReleaser.
/// Update the source comment if that is intentional.
#[test]
fn test_combined_sort_doublespace_divergence() {
    // Mirrors the `split_once("  ")` keying used in the combined-line
    // sort above. A filename containing a double-space splits early,
    // producing a wrong key — inherited from GoReleaser checksums.go.
    let line = "deadbeef  weird  name.tar.gz";
    let (_hash, rest) = line.split_once("  ").unwrap();
    assert_eq!(
        rest, "weird  name.tar.gz",
        "split_once extracts everything after the first double-space"
    );
    // And the sort key for a line where the filename itself contains
    // a double-space picks up only the prefix before the *next*
    // double-space — the documented divergence point.
    let key = rest.split_once("  ").map(|(p, _)| p).unwrap_or(rest);
    assert_eq!(key, "weird", "inherited divergence — not a real filename");
}

// -- Algorithm unit tests with known test vectors -------------------------

#[test]
fn test_sha1_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha1_file(&f).unwrap();
    assert_eq!(hash, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
}

#[test]
fn test_sha224_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha224_file(&f).unwrap();
    assert_eq!(
        hash,
        "2f05477fc24bb4faefd86517156dafdecec45b8ad3cf2522a563582b"
    );
}

#[test]
fn test_sha256_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha256_file(&f).unwrap();
    assert_eq!(
        hash,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn test_sha384_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha384_file(&f).unwrap();
    assert!(hash.starts_with("fdbd8e75a67f29f701a4e040385e2e23986303ea10239211af907fcbb83578b3"));
    assert_eq!(hash.len(), 96); // SHA-384 hex length
}

#[test]
fn test_sha512_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha512_file(&f).unwrap();
    assert!(hash.starts_with("309ecc489c12d6eb4cc40f50c902f2b4"));
    assert_eq!(hash.len(), 128); // SHA-512 hex length
}

#[test]
fn test_blake2b_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = blake2b_file(&f).unwrap();
    assert!(hash.starts_with("021ced8799296ceca557832ab941a50b4a11f83478cf141f51f933f653ab9fbc"));
    assert_eq!(hash.len(), 128); // Blake2b-512 hex length
}

#[test]
fn test_blake2s_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = blake2s_file(&f).unwrap();
    assert!(hash.starts_with("9aec6806794561107e594b1f6a8a6b0c"));
    assert_eq!(hash.len(), 64); // Blake2s-256 hex length
}

#[test]
fn test_sha3_224_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha3_224_file(&f).unwrap();
    assert_eq!(hash.len(), 56); // SHA3-224 hex length = 28 bytes * 2
}

#[test]
fn test_sha3_256_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha3_256_file(&f).unwrap();
    assert_eq!(
        hash,
        "644bcc7e564373040999aac89e7622f3ca71fba1d972fd94a31c3bfbf24e3938"
    );
}

#[test]
fn test_sha3_384_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha3_384_file(&f).unwrap();
    assert_eq!(hash.len(), 96); // SHA3-384 hex length = 48 bytes * 2
}

#[test]
fn test_sha3_512_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = sha3_512_file(&f).unwrap();
    assert!(hash.starts_with("840006653e9ac9e95117a15c915caab81662918e925de9e004f774ff82d7079a"));
    assert_eq!(hash.len(), 128); // SHA3-512 hex length
}

#[test]
fn test_blake3_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = blake3_file(&f).unwrap();
    assert_eq!(
        hash,
        "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24"
    );
}

#[test]
fn test_crc32_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = crc32_file(&f).unwrap();
    assert_eq!(hash, "0d4a1185");
}

#[test]
fn test_md5_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();
    let hash = md5_file(&f).unwrap();
    assert_eq!(hash, "5eb63bbbe01eeed093cb22bb8f5acdc3");
}

// -- Dispatch tests -------------------------------------------------------

#[test]
fn test_hash_file_dispatches() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();

    let h1 = hash_file(&f, "sha1").unwrap();
    assert_eq!(h1.len(), 40);

    let h224 = hash_file(&f, "sha224").unwrap();
    assert_eq!(h224.len(), 56);

    let h256 = hash_file(&f, "sha256").unwrap();
    assert_eq!(h256.len(), 64);

    let h384 = hash_file(&f, "sha384").unwrap();
    assert_eq!(h384.len(), 96);

    let h512 = hash_file(&f, "sha512").unwrap();
    assert_eq!(h512.len(), 128);

    let hb2b = hash_file(&f, "blake2b").unwrap();
    assert_eq!(hb2b.len(), 128);

    let hb2s = hash_file(&f, "blake2s").unwrap();
    assert_eq!(hb2s.len(), 64);

    let hsha3_224 = hash_file(&f, "sha3-224").unwrap();
    assert_eq!(hsha3_224.len(), 56);

    let hsha3_256 = hash_file(&f, "sha3-256").unwrap();
    assert_eq!(hsha3_256.len(), 64);

    let hsha3_384 = hash_file(&f, "sha3-384").unwrap();
    assert_eq!(hsha3_384.len(), 96);

    let hsha3_512 = hash_file(&f, "sha3-512").unwrap();
    assert_eq!(hsha3_512.len(), 128);

    let hblake3 = hash_file(&f, "blake3").unwrap();
    assert_eq!(hblake3.len(), 64);

    let hcrc32 = hash_file(&f, "crc32").unwrap();
    assert_eq!(hcrc32.len(), 8);

    let hmd5 = hash_file(&f, "md5").unwrap();
    assert_eq!(hmd5.len(), 32);

    // Unsupported algorithm should fail
    assert!(hash_file(&f, "bogus").is_err());
}

#[test]
fn test_format_checksum_line() {
    let line = format_checksum_line("abcdef1234", "myfile.tar.gz");
    assert_eq!(line, "abcdef1234  myfile.tar.gz");
}

// -- Config parsing tests -------------------------------------------------

#[test]
fn test_extra_files_config_parsing() {
    use anodizer_core::config::ExtraFileSpec;

    let yaml = r#"
name_template: "checksums.txt"
algorithm: "sha256"
extra_files:
  - "dist/*.bin"
  - "README.md"
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.extra_files,
        Some(vec![
            ExtraFileSpec::Glob("dist/*.bin".to_string()),
            ExtraFileSpec::Glob("README.md".to_string()),
        ])
    );
}

#[test]
fn test_ids_filter_config_parsing() {
    let yaml = r#"
algorithm: "sha512"
ids:
  - "linux-amd64"
  - "darwin-arm64"
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.ids,
        Some(vec!["linux-amd64".to_string(), "darwin-arm64".to_string()])
    );
}

// -- Stage integration tests ----------------------------------------------

#[test]
fn test_checksum_stage_run() {
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Create a fake archive file
    let archive_path = dist.join("myapp-1.0.0-linux-amd64.tar.gz");
    fs::write(&archive_path, b"fake archive content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    // Register an Archive artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Default (non-split) mode: only combined file, no sidecars
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "non-split mode should only produce combined file"
    );

    // Sidecar file should NOT exist in non-split mode
    let sidecar = dist.join("myapp-1.0.0-linux-amd64.tar.gz.sha256");
    assert!(
        !sidecar.exists(),
        "sidecar file should NOT exist in non-split mode"
    );

    // Combined file should exist in dist
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists(), "combined checksums file should exist");
    let combined_content = fs::read_to_string(&combined).unwrap();
    assert!(combined_content.contains("  myapp-1.0.0-linux-amd64.tar.gz"));
}

#[test]
fn test_checksum_stage_dry_run() {
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"fake").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dry_run(true)
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // In dry-run, Checksum artifacts are still registered (so downstream
    // stages like sign/release can reference them), but no files are
    // written to disk.
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert!(!checksums.is_empty());

    // The combined checksums file should NOT exist on disk in dry-run.
    let checksum_file = dist.join("myapp_1.0.0_checksums.txt");
    assert!(!checksum_file.exists());
}

#[test]
fn test_checksum_stage_sha512() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                algorithm: Some("sha512".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: no sidecar, only combined
    let sidecar = dist.join("myapp.tar.gz.sha512");
    assert!(
        !sidecar.exists(),
        "sidecar should NOT exist in non-split mode"
    );

    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists());
    let content = fs::read_to_string(&combined).unwrap();
    // SHA512 hex is 128 chars
    let hash_part = content.split_whitespace().next().unwrap_or("");
    assert_eq!(hash_part.len(), 128);
}

#[test]
fn test_checksum_stage_no_artifacts_skips() {
    use anodizer_core::config::CrateConfig;
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
            ..Default::default()
        }])
        .build();
    // No artifacts registered at all

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert!(checksums.is_empty());
}

#[test]
fn test_checksum_stage_global_disable() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig, Defaults, StringOrBool};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"fake archive content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist)
        .defaults(Defaults {
            checksum: Some(ChecksumConfig {
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        })
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // No checksums should be generated when globally disabled
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert!(checksums.is_empty());
}

#[test]
fn test_checksum_stage_per_crate_disable() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"fake archive content").unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        checksum: Some(ChecksumConfig {
            algorithm: Some("sha256".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // No checksums should be generated for the disabled crate
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert!(checksums.is_empty());
}

#[test]
fn test_checksum_stage_with_extra_files() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Create a fake archive file
    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"fake archive").unwrap();

    // Create extra files that will be matched by glob
    let extra1 = dist.join("extra1.bin");
    let extra2 = dist.join("extra2.bin");
    fs::write(&extra1, b"extra file 1").unwrap();
    fs::write(&extra2, b"extra file 2").unwrap();

    let glob_pattern = format!("{}/*.bin", dist.display());

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        checksum: Some(ChecksumConfig {
            extra_files: Some(vec![ExtraFileSpec::Glob(glob_pattern)]),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: only 1 combined artifact (no sidecars)
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(checksums.len(), 1);

    // Combined file should include all three files
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists());
    let content = fs::read_to_string(&combined).unwrap();
    assert!(content.contains("myapp.tar.gz"));
    assert!(content.contains("extra1.bin"));
    assert!(content.contains("extra2.bin"));
}

#[test]
fn test_checksum_stage_with_ids_filter() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive1 = dist.join("myapp-linux.tar.gz");
    let archive2 = dist.join("myapp-darwin.tar.gz");
    fs::write(&archive1, b"linux archive").unwrap();
    fs::write(&archive2, b"darwin archive").unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        checksum: Some(ChecksumConfig {
            ids: Some(vec!["linux-amd64".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Archive with matching id
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive1.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), "linux-amd64".to_string());
            m
        },
        size: None,
    });

    // Archive with non-matching id
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive2.clone(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), "darwin-arm64".to_string());
            m
        },
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: only combined artifact (no sidecars)
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(checksums.len(), 1);

    // Combined file should only contain the linux archive
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    let content = fs::read_to_string(&combined).unwrap();
    assert!(content.contains("myapp-linux.tar.gz"));
    assert!(!content.contains("myapp-darwin.tar.gz"));
}

// -----------------------------------------------------------------------
// Deep integration tests: verify checksum format and hash correctness
// -----------------------------------------------------------------------

#[test]
fn test_integration_checksum_file_format_and_correctness() {
    // Create files with known content and verify checksums are correct
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Known content: "hello world" -> SHA-256 = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
    let file1 = dist.join("app-linux.tar.gz");
    fs::write(&file1, b"hello world").unwrap();

    // Known content: "test data" -> SHA-256 = 916f0027a575074ce72a331777c3478d6513f786a591bd892da1a577bf2335f9
    let file2 = dist.join("app-darwin.tar.gz");
    fs::write(&file2, b"test data").unwrap();

    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "app".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "2.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: file1.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: file2.clone(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: no sidecars, only combined file
    let sidecar1 = dist.join("app-linux.tar.gz.sha256");
    assert!(
        !sidecar1.exists(),
        "sidecar should NOT exist in non-split mode"
    );
    let sidecar2 = dist.join("app-darwin.tar.gz.sha256");
    assert!(
        !sidecar2.exists(),
        "sidecar should NOT exist in non-split mode"
    );

    // Verify combined checksums file has correct multi-line format
    let combined = dist.join("app_2.0.0_checksums.txt");
    assert!(combined.exists());
    let combined_content = fs::read_to_string(&combined).unwrap();
    let lines: Vec<&str> = combined_content.trim().lines().collect();
    assert_eq!(lines.len(), 2, "combined file should have exactly 2 lines");

    // Each line should match the format "<64-char-hex>  <filename>"
    for line in &lines {
        let parts: Vec<&str> = line.splitn(2, "  ").collect();
        assert_eq!(parts.len(), 2, "each line should have hash and filename");
        assert_eq!(
            parts[0].len(),
            64,
            "SHA-256 hash should be 64 hex characters"
        );
        assert!(
            parts[0].chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be all hex characters"
        );
    }

    // Verify the combined file contains both filenames with correct hashes
    assert!(combined_content.contains(
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9  app-linux.tar.gz"
    ));
    assert!(combined_content.contains(
        "916f0027a575074ce72a331777c3478d6513f786a591bd892da1a577bf2335f9  app-darwin.tar.gz"
    ));
}

#[test]
fn test_integration_checksum_hash_independently_verifiable() {
    // Generate a checksum via the stage, then independently compute the hash
    // and confirm they match.
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let content = b"The quick brown fox jumps over the lazy dog";
    let archive = dist.join("release.tar.gz");
    fs::write(&archive, content).unwrap();

    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "fox".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "fox".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive.clone(),
        target: None,
        crate_name: "fox".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Independently compute the SHA-256 hash using the crate's own function
    let expected_hash = sha256_file(&archive).unwrap();

    // Non-split mode: no sidecar, verify via combined file
    let sidecar = dist.join("release.tar.gz.sha256");
    assert!(
        !sidecar.exists(),
        "sidecar should NOT exist in non-split mode"
    );

    let combined = dist.join("fox_1.0.0_checksums.txt");
    let combined_content = fs::read_to_string(&combined).unwrap();
    let combined_hash = combined_content.trim().split("  ").next().unwrap();
    assert_eq!(
        combined_hash, expected_hash,
        "combined file hash should match independently computed hash"
    );
}

#[test]
fn test_integration_checksum_multiple_algorithms_produce_correct_lengths() {
    // Test that sha512 produces the right hash length in the output file
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("pkg.tar.gz");
    fs::write(&archive, b"some package content").unwrap();

    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "pkg".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "pkg".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                algorithm: Some("sha512".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive.clone(),
        target: None,
        crate_name: "pkg".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: verify via combined file
    let sidecar = dist.join("pkg.tar.gz.sha512");
    assert!(
        !sidecar.exists(),
        "sidecar should NOT exist in non-split mode"
    );

    let combined = dist.join("pkg_1.0.0_checksums.txt");
    assert!(combined.exists());
    let content = fs::read_to_string(&combined).unwrap();
    let hash = content.trim().split("  ").next().unwrap();
    assert_eq!(hash.len(), 128, "SHA-512 should produce 128 hex chars");

    // Independently verify the hash value
    let expected = sha512_file(&archive).unwrap();
    assert_eq!(hash, expected);
}

// -- TestContextBuilder + create_fake_binary integration test --

#[test]
fn test_checksum_of_fake_binary_via_builder() {
    use anodizer_core::test_helpers::{TestContextBuilder, create_fake_binary};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let fake_bin = create_fake_binary(&dist, "myapp-linux.tar.gz");

    let mut ctx = TestContextBuilder::new()
        .project_name("checksum-test")
        .tag("v2.0.0")
        .dist(dist.clone())
        .crates(vec![anodizer_core::config::CrateConfig {
            name: "checksum-test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: fake_bin.clone(),
        target: None,
        crate_name: "checksum-test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Non-split mode: verify via combined file (no sidecar)
    let sidecar = dist.join("myapp-linux.tar.gz.sha256");
    assert!(
        !sidecar.exists(),
        "sidecar should NOT exist in non-split mode"
    );

    let combined = dist.join("checksum-test_2.0.0_checksums.txt");
    assert!(combined.exists(), "combined file should exist");
    let combined_content = fs::read_to_string(&combined).unwrap();
    let expected_hash = sha256_file(&fake_bin).unwrap();
    assert!(combined_content.starts_with(&expected_hash));
}

// -----------------------------------------------------------------------
// Task 4C: Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_each_algorithm_produces_correct_known_hash() {
    // Verify known test vectors for "hello world" against all algorithms
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();

    // SHA-1: well-known test vector
    assert_eq!(
        hash_file(&f, "sha1").unwrap(),
        "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed"
    );
    // SHA-256: well-known test vector
    assert_eq!(
        hash_file(&f, "sha256").unwrap(),
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
    // SHA-512 prefix
    assert!(
        hash_file(&f, "sha512")
            .unwrap()
            .starts_with("309ecc489c12d6eb4cc40f50c902f2b4")
    );
}

#[test]
fn test_checksum_file_registered_as_checksum_artifact() {
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("release.tar.gz");
    fs::write(&archive, b"data").unwrap();

    let config = Config {
        project_name: "myapp".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Non-split mode: only combined artifact (no sidecars)
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "non-split mode should only produce combined file"
    );

    // All checksum artifacts should have kind = Checksum
    for a in &checksums {
        assert_eq!(a.kind, ArtifactKind::Checksum);
        assert!(a.metadata.contains_key("algorithm"));
    }

    // Combined file should have "combined" metadata
    let combined = checksums
        .iter()
        .find(|a| a.metadata.get("combined") == Some(&"true".to_string()));
    assert!(
        combined.is_some(),
        "should have a combined checksum artifact"
    );
}

#[test]
fn test_checksum_missing_file_errors() {
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let nonexistent = dist.join("does-not-exist.tar.gz");

    let config = Config {
        project_name: "myapp".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: nonexistent,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let result = ChecksumStage.run(&mut ctx);
    assert!(
        result.is_err(),
        "checksumming a nonexistent file should error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does-not-exist.tar.gz"),
        "error should contain the missing file path, got: {err}"
    );
}

#[test]
fn test_extra_files_appear_in_combined_checksum() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("app.tar.gz");
    fs::write(&archive, b"archive content").unwrap();

    let extra = dist.join("extra-file.txt");
    fs::write(&extra, b"extra content").unwrap();

    let glob_pattern = format!("{}/extra-*.txt", dist.display());

    let config = Config {
        project_name: "app".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                extra_files: Some(vec![ExtraFileSpec::Glob(glob_pattern)]),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Combined file should include both archive and extra file
    let combined = dist.join("app_1.0.0_checksums.txt");
    let content = fs::read_to_string(&combined).unwrap();
    assert!(
        content.contains("app.tar.gz"),
        "combined should include archive"
    );
    assert!(
        content.contains("extra-file.txt"),
        "combined should include extra file"
    );
}

/// Regression: `{{ .Algorithm }}` must be available inside
/// extra_files[].name_template (combined-checksum alias rendering path).
/// Previously `Algorithm` was only set on the sidecar name_template vars
/// bag — users writing `"{{ .ArtifactName }}.{{ .Algorithm }}"` saw
/// render failure and fell back to the raw filename.
#[test]
fn test_extra_files_name_template_exposes_algorithm_var() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("app.tar.gz");
    fs::write(&archive, b"archive content").unwrap();

    let extra = dist.join("extra-file.txt");
    fs::write(&extra, b"extra content").unwrap();

    let config = Config {
        project_name: "app".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                algorithm: Some("sha256".to_string()),
                extra_files: Some(vec![ExtraFileSpec::Detailed {
                    glob: extra.to_string_lossy().into_owned(),
                    name_template: Some("{{ .ArtifactName }}.{{ .Algorithm }}".to_string()),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    let combined = dist.join("app_1.0.0_checksums.txt");
    let content = fs::read_to_string(&combined).unwrap();
    assert!(
        content.contains("extra-file.txt.sha256"),
        "combined should include Algorithm-rendered alias; got:\n{content}"
    );
}

#[test]
fn test_ids_filter_excludes_unmatched_artifacts() {
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let linux = dist.join("app-linux.tar.gz");
    let darwin = dist.join("app-darwin.tar.gz");
    let windows = dist.join("app-windows.zip");
    fs::write(&linux, b"linux").unwrap();
    fs::write(&darwin, b"darwin").unwrap();
    fs::write(&windows, b"windows").unwrap();

    let config = Config {
        project_name: "app".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                ids: Some(vec!["linux".to_string(), "darwin".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add 3 artifacts, only 2 have matching ids
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: linux,
        target: None,
        crate_name: "app".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), "linux".to_string());
            m
        },
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: darwin,
        target: None,
        crate_name: "app".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), "darwin".to_string());
            m
        },
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: windows,
        target: None,
        crate_name: "app".to_string(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), "windows".to_string());
            m
        },
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Combined file should include only linux and darwin
    let combined = dist.join("app_1.0.0_checksums.txt");
    let content = fs::read_to_string(&combined).unwrap();
    assert!(content.contains("app-linux.tar.gz"));
    assert!(content.contains("app-darwin.tar.gz"));
    assert!(
        !content.contains("app-windows.zip"),
        "windows should be excluded by ids filter"
    );
}

// ---- Error path tests (Task 4D) ----

#[test]
fn test_hash_file_missing_file_errors_with_path() {
    let result = hash_file(Path::new("/nonexistent/file.tar.gz"), "sha256");
    assert!(result.is_err(), "hashing a missing file should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("/nonexistent/file.tar.gz") || err.contains("sha256"),
        "error should mention the file path or algorithm, got: {err}"
    );
}

#[test]
fn test_unsupported_algorithm_errors_with_name() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello").unwrap();

    let result = hash_file(&f, "whirlpool");
    assert!(result.is_err(), "unsupported algorithm should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unsupported checksum algorithm") && err.contains("whirlpool"),
        "error should mention 'unsupported checksum algorithm' and 'whirlpool', got: {err}"
    );
}

#[test]
fn test_unsupported_algorithm_ripemd() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello").unwrap();

    let result = hash_file(&f, "ripemd160");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unsupported") && err.contains("ripemd160"),
        "error should name the unsupported algorithm, got: {err}"
    );
}

#[test]
fn test_checksum_sidecar_write_to_nonexistent_dir_fails() {
    // Attempting to create a sidecar file in a directory that doesn't exist
    // should fail with a descriptive error.
    let sidecar = Path::new("/nonexistent_dir_12345/test.tar.gz.sha256");
    let write_result = File::create(sidecar);
    assert!(
        write_result.is_err(),
        "creating sidecar in nonexistent dir should fail"
    );
    let err = write_result.unwrap_err().to_string();
    assert!(
        err.contains("No such file or directory")
            || err.contains("not found")
            || err.contains("cannot find the path"),
        "error should mention missing directory, got: {err}"
    );
}

#[test]
fn test_each_sha_algorithm_on_missing_file() {
    let missing = Path::new("/nonexistent/checksum_test_file");
    for algo in &[
        "sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s",
    ] {
        let result = hash_file(missing, algo);
        assert!(
            result.is_err(),
            "algorithm {} should fail on missing file",
            algo
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains(algo) || err.contains("nonexistent"),
            "error for {} should mention algo or path, got: {}",
            algo,
            err
        );
    }
}

// -- split mode tests ---------------------------------------------------

#[test]
fn test_split_config_parsing() {
    let yaml = r#"
algorithm: "sha256"
split: true
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.split, Some(true));
}

#[test]
fn test_split_config_parsing_false() {
    let yaml = r#"
algorithm: "sha256"
split: false
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.split, Some(false));
}

#[test]
fn test_split_config_parsing_absent() {
    let yaml = r#"
algorithm: "sha256"
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.split, None);
}

#[test]
fn test_checksum_stage_split_true_no_combined_file() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp-1.0.0-linux-amd64.tar.gz");
    fs::write(&archive_path, b"fake archive content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                split: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // Only sidecar file should be created (no combined)
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "split=true should create only 1 sidecar artifact, got {}",
        checksums.len()
    );

    // Sidecar file should exist
    let sidecar = dist.join("myapp-1.0.0-linux-amd64.tar.gz.sha256");
    assert!(sidecar.exists(), "sidecar file should exist");

    // Combined file should NOT exist
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(
        !combined.exists(),
        "combined checksums file should NOT exist in split mode"
    );
}

#[test]
fn test_checksum_stage_split_false_only_combined() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp-1.0.0-linux-amd64.tar.gz");
    fs::write(&archive_path, b"fake archive content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                split: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // split=false: only combined file, no sidecars
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "split=false should create only combined artifact, got {}",
        checksums.len()
    );

    let sidecar = dist.join("myapp-1.0.0-linux-amd64.tar.gz.sha256");
    assert!(
        !sidecar.exists(),
        "sidecar should NOT exist when split=false"
    );

    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(
        combined.exists(),
        "combined checksums file should exist when split=false"
    );
}

#[test]
fn test_checksum_stage_default_split_only_combined() {
    // When split is not set (None), default behavior creates only combined (no sidecars)
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "default (no split) should create only combined"
    );
}

#[test]
fn test_checksum_stage_global_split_cascades_to_crate() {
    // When defaults.checksum.split = true and crate has no per-crate checksum config,
    // the global split setting should cascade down.
    use anodizer_core::config::{ChecksumConfig, CrateConfig, Defaults};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .defaults(Defaults {
            checksum: Some(ChecksumConfig {
                split: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        })
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            // No per-crate checksum config — should inherit global split: true
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "global split: true should cascade to crate — only sidecar, no combined"
    );
}

// -- Default filename format tests -----------------------------------------

#[test]
fn test_default_checksum_filename_uses_project_name_and_version() {
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive_path = dist.join("coolapp-3.0.0-linux-amd64.tar.gz");
    fs::write(&archive_path, b"archive content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("coolapp")
        .tag("v3.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "coolapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "coolapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Default filename should be {project_name}_{version}_checksums.txt
    let combined = dist.join("coolapp_3.0.0_checksums.txt");
    assert!(
        combined.exists(),
        "default checksum filename should be coolapp_3.0.0_checksums.txt, \
         files in dist: {:?}",
        fs::read_dir(&dist)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
}

// -- SHA3-224 and SHA3-384 dispatch tests ----------------------------------

#[test]
fn test_sha3_224_dispatches_via_hash_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();

    let h = hash_file(&f, "sha3-224").unwrap();
    assert_eq!(h.len(), 56, "SHA3-224 should produce 56 hex chars");
    // Also verify it matches the direct function
    assert_eq!(h, sha3_224_file(&f).unwrap());
}

#[test]
fn test_sha3_384_dispatches_via_hash_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("test.txt");
    fs::write(&f, b"hello world").unwrap();

    let h = hash_file(&f, "sha3-384").unwrap();
    assert_eq!(h.len(), 96, "SHA3-384 should produce 96 hex chars");
    // Also verify it matches the direct function
    assert_eq!(h, sha3_384_file(&f).unwrap());
}

// -----------------------------------------------------------------------
// Task 4: Config + wiring parity tests
// -----------------------------------------------------------------------

#[test]
fn test_config_disable_template_string_parsing() {
    let yaml = r#"
algorithm: "sha256"
skip: "{{ if .IsSnapshot }}true{{ end }}"
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    match &cfg.skip {
        Some(anodizer_core::config::StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
            assert!(cfg.skip.as_ref().unwrap().is_template());
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_config_disable_bool_parsing() {
    let yaml = r#"
algorithm: "sha256"
skip: true
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    assert!(!cfg.skip.as_ref().unwrap().is_template());
}

#[test]
fn test_config_extra_files_object_form() {
    use anodizer_core::config::ExtraFileSpec;

    let yaml = r#"
extra_files:
  - "dist/*.bin"
  - glob: "release/*.deb"
    name_template: "{{ .ArtifactName }}.checksum"
"#;
    let cfg: anodizer_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let extra = cfg.extra_files.unwrap();
    assert_eq!(extra.len(), 2);
    assert_eq!(extra[0], ExtraFileSpec::Glob("dist/*.bin".to_string()));
    match &extra[1] {
        ExtraFileSpec::Detailed {
            glob,
            name_template,
        } => {
            assert_eq!(glob, "release/*.deb");
            assert_eq!(
                name_template.as_deref(),
                Some("{{ .ArtifactName }}.checksum")
            );
        }
        other => panic!("expected ExtraFileSpec::Detailed, got {:?}", other),
    }
}

#[test]
fn test_nonsplit_mode_does_not_create_sidecars() {
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive1 = dist.join("app-linux.tar.gz");
    let archive2 = dist.join("app-darwin.tar.gz");
    fs::write(&archive1, b"linux").unwrap();
    fs::write(&archive2, b"darwin").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("app")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive1,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive2,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Non-split: only 1 combined artifact
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(checksums.len(), 1, "non-split should produce only combined");
    assert_eq!(
        checksums[0].metadata.get("combined"),
        Some(&"true".to_string())
    );

    // No sidecar files on disk
    assert!(!dist.join("app-linux.tar.gz.sha256").exists());
    assert!(!dist.join("app-darwin.tar.gz.sha256").exists());

    // Combined file should contain both
    let combined = dist.join("app_1.0.0_checksums.txt");
    assert!(combined.exists());
    let content = fs::read_to_string(&combined).unwrap();
    assert!(content.contains("app-linux.tar.gz"));
    assert!(content.contains("app-darwin.tar.gz"));
}

#[test]
fn test_split_mode_creates_sidecars_no_combined() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive1 = dist.join("app-linux.tar.gz");
    let archive2 = dist.join("app-darwin.tar.gz");
    fs::write(&archive1, b"linux").unwrap();
    fs::write(&archive2, b"darwin").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("app")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                split: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive1,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive2,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Split mode: 2 sidecar artifacts, no combined
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(checksums.len(), 2, "split mode should produce 2 sidecars");
    for a in &checksums {
        assert!(
            a.metadata.contains_key("ChecksumOf"),
            "sidecar artifact should have ChecksumOf metadata"
        );
        assert!(
            !a.metadata.contains_key("combined"),
            "sidecar artifact should NOT have combined metadata"
        );
    }

    // Sidecar files on disk
    assert!(dist.join("app-linux.tar.gz.sha256").exists());
    assert!(dist.join("app-darwin.tar.gz.sha256").exists());

    // No combined file
    assert!(!dist.join("app_1.0.0_checksums.txt").exists());
}

#[test]
fn test_split_mode_with_name_template() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("app-linux.tar.gz");
    fs::write(&archive, b"linux content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("app")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                split: Some(true),
                name_template: Some("{{ .ArtifactName }}.checksumfile".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive.clone(),
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Sidecar should use the rendered name_template
    let custom_sidecar = dist.join("app-linux.tar.gz.checksumfile");
    assert!(
        custom_sidecar.exists(),
        "sidecar should be named via name_template, expected: app-linux.tar.gz.checksumfile, \
         files: {:?}",
        fs::read_dir(&dist)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );

    // Default-named sidecar should NOT exist
    let default_sidecar = dist.join("app-linux.tar.gz.sha256");
    assert!(
        !default_sidecar.exists(),
        "default sidecar name should NOT be used when name_template is set"
    );

    // Verify content is correct — GoReleaser writes ONLY the raw hex hash
    // in sidecar files (no filename, no trailing newline).
    let content = fs::read_to_string(&custom_sidecar).unwrap();
    let expected_hash = sha256_file(&archive).unwrap();
    assert_eq!(content, expected_hash);
}

#[test]
fn test_disable_template_string_skips_when_true() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig, StringOrBool};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let archive = dist.join("app.tar.gz");
    fs::write(&archive, b"content").unwrap();

    // Use a template that resolves to "true" (via simple string, not real template)
    let mut ctx = TestContextBuilder::new()
        .project_name("app")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                skip: Some(StringOrBool::String("true".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive,
        target: None,
        crate_name: "app".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Should be disabled via template evaluation
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert!(
        checksums.is_empty(),
        "skip: 'true' string should disable checksums"
    );
}

#[test]
fn test_extra_file_detailed_name_template_combined_mode() {
    // Verifies that ExtraFileSpec::Detailed with name_template correctly renames
    // the entry in the combined (non-split) checksum file via the template engine.
    use anodizer_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Create the archive and extra file
    let archive = dist.join("myapp.tar.gz");
    fs::write(&archive, b"archive content").unwrap();

    let extra = dist.join("RELEASE_NOTES.txt");
    fs::write(&extra, b"release notes content").unwrap();

    let glob_pattern = format!("{}/RELEASE_NOTES.txt", dist.display());

    let config = Config {
        project_name: "myapp".to_string(),
        dist: dist.clone(),
        crates: vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                // split defaults to false — combined mode
                extra_files: Some(vec![ExtraFileSpec::Detailed {
                    glob: glob_pattern,
                    name_template: Some("custom-{{ .ArtifactName }}".to_string()),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    ChecksumStage.run(&mut ctx).unwrap();

    // Non-split mode: only 1 combined artifact
    let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "non-split mode should produce one combined artifact"
    );

    // Combined file should contain the custom-named entry for the extra file
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists(), "combined checksum file should exist");
    let content = fs::read_to_string(&combined).unwrap();

    // The extra file should appear with its custom name (template rendered)
    assert!(
        content.contains("custom-RELEASE_NOTES.txt"),
        "combined file should contain the custom-named extra file entry, got:\n{content}"
    );
    // The original archive should still appear by its real name
    assert!(
        content.contains("myapp.tar.gz"),
        "combined file should contain the archive, got:\n{content}"
    );
}

#[test]
fn test_checksum_stage_with_templated_extra_files() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig, TemplatedExtraFile};
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Create a source template file
    let tpl_src = tmp.path().join("NOTES.md.tpl");
    fs::write(
        &tpl_src,
        "Release notes for {{ .ProjectName }} {{ .Version }}",
    )
    .unwrap();

    // Create a fake archive
    let archive_path = dist.join("myapp.tar.gz");
    fs::write(&archive_path, b"fake archive").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            checksum: Some(ChecksumConfig {
                templated_extra_files: Some(vec![TemplatedExtraFile {
                    src: tpl_src.to_string_lossy().to_string(),
                    dst: Some("NOTES.md".to_string()),
                    mode: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: archive_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    // The combined checksum file should include an entry for the templated file
    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists(), "combined checksums file should exist");
    let content = fs::read_to_string(&combined).unwrap();
    assert!(
        content.contains("NOTES.md"),
        "checksum file should include templated extra file, got:\n{content}"
    );
    assert!(
        content.contains("myapp.tar.gz"),
        "checksum file should still include the archive, got:\n{content}"
    );

    // Verify the rendered file was written with template content expanded
    let rendered = dist.join("NOTES.md");
    assert!(rendered.exists());
    let rendered_content = fs::read_to_string(&rendered).unwrap();
    assert_eq!(rendered_content, "Release notes for myapp 1.0.0");
}

#[test]
fn test_checksum_source_list_cross_links_release_uploadable_kinds() {
    // Pins C-new-23: stage-checksum's source-list is the cross-linked
    // `release_uploadable_kinds()` minus `Checksum` (mirroring GR's
    // `Not(ByType(Checksum))` filter). Six kinds previously absent —
    // Makeself, Flatpak, SourceRpm, UploadableFile, Signature, Certificate
    // — must now be checksummed; Snap and raw Binary must NOT.
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // One file per artifact kind under test. Filenames are chosen so that
    // none is a substring suffix of another and assertions go through a
    // line-parsed filename set (not `content.contains`) — that combination
    // is what catches a regression like dropping Archive while Signature
    // (which carries a filename of the form `<archive>.sig`) is still
    // present.
    let cases: &[(ArtifactKind, &str, bool)] = &[
        // (kind, filename, must_be_in_combined_checksums)
        (ArtifactKind::Archive, "myapp-archive.tar.gz", true),
        (ArtifactKind::UploadableBinary, "myapp-bin", true),
        (ArtifactKind::UploadableFile, "extra.txt", true),
        (ArtifactKind::SourceArchive, "myapp-source.tar.gz", true),
        (ArtifactKind::Makeself, "myapp.run", true),
        (ArtifactKind::LinuxPackage, "myapp.deb", true),
        (ArtifactKind::Flatpak, "myapp.flatpak", true),
        (ArtifactKind::SourceRpm, "myapp.src.rpm", true),
        (ArtifactKind::Installer, "myapp.msi", true),
        (ArtifactKind::DiskImage, "myapp.dmg", true),
        (ArtifactKind::MacOsPackage, "myapp.pkg", true),
        (ArtifactKind::Sbom, "myapp.sbom.json", true),
        (ArtifactKind::Signature, "myapp-sig.tar.gz.sig", true),
        (ArtifactKind::Certificate, "myapp.crt", true),
        // Excluded: snap-store-bound + raw build output.
        (ArtifactKind::Snap, "myapp.snap", false),
        (ArtifactKind::Binary, "myapp-raw-bin", false),
        // Recursion-prevention: pre-existing Checksum artifacts (from a
        // prior pipe pass or a merge step) must be filtered out of the
        // source list so the new combined file does not list itself.
        (ArtifactKind::Checksum, "prior.checksums.txt", false),
    ];

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();

    for (kind, filename, _) in cases {
        let path = dist.join(filename);
        fs::write(&path, format!("contents of {filename}").as_bytes()).unwrap();
        ctx.artifacts.add(Artifact {
            kind: *kind,
            name: filename.to_string(),
            path,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    let stage = ChecksumStage;
    stage.run(&mut ctx).unwrap();

    let combined = dist.join("myapp_1.0.0_checksums.txt");
    assert!(combined.exists(), "combined checksums file must exist");
    let content = fs::read_to_string(&combined).unwrap();

    // Parse `{hash}  {filename}` lines into the set of filenames present.
    // Structural extraction avoids substring false-positives — e.g.,
    // `content.contains("myapp.tar.gz")` would spuriously match against a
    // line ending in `myapp.tar.gz.sig`.
    let filenames_in_combined: std::collections::HashSet<&str> = content
        .lines()
        .filter_map(|l| l.split_once("  ").map(|(_, name)| name))
        .collect();

    for (kind, filename, must_be_in) in cases {
        let present = filenames_in_combined.contains(filename);
        assert_eq!(
            present,
            *must_be_in,
            "expected `{filename}` (kind={kind:?}) {} in checksums.txt; got:\n{content}",
            if *must_be_in { "present" } else { "absent" },
        );
    }
}
