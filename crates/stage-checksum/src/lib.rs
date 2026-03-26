use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use blake2::{Blake2b512, Blake2s256};
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::ArchivesConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Hash helpers
// ---------------------------------------------------------------------------

/// Generic helper: open a file, feed it to any `Digest` hasher, return hex.
fn hash_file_with<D: Digest>(path: &Path, algo_name: &str) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("{algo_name}: open {}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("{algo_name}: read {}", path.display()))?;
        if n == 0 {
            break;
        }
        Digest::update(&mut hasher, &buf[..n]);
    }
    let result = hasher.finalize();
    let hex: String = result.iter().map(|b| format!("{:02x}", b)).collect();
    Ok(hex)
}

pub fn sha1_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha1>(path, "sha1")
}

pub fn sha224_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha224>(path, "sha224")
}

pub fn sha256_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha256>(path, "sha256")
}

pub fn sha384_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha384>(path, "sha384")
}

pub fn sha512_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha512>(path, "sha512")
}

pub fn blake2b_file(path: &Path) -> Result<String> {
    hash_file_with::<Blake2b512>(path, "blake2b")
}

pub fn blake2s_file(path: &Path) -> Result<String> {
    hash_file_with::<Blake2s256>(path, "blake2s")
}

pub fn hash_file(path: &Path, algorithm: &str) -> Result<String> {
    match algorithm {
        "sha1" => sha1_file(path),
        "sha224" => sha224_file(path),
        "sha256" => sha256_file(path),
        "sha384" => sha384_file(path),
        "sha512" => sha512_file(path),
        "blake2b" => blake2b_file(path),
        "blake2s" => blake2s_file(path),
        _ => bail!("unsupported checksum algorithm: {}", algorithm),
    }
}

pub fn format_checksum_line(hash: &str, filename: &str) -> String {
    format!("{}  {}", hash, filename)
}

// ---------------------------------------------------------------------------
// Extra-files glob resolution
// ---------------------------------------------------------------------------

/// Resolve a list of glob patterns into deduplicated, sorted file paths.
fn resolve_extra_files(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .with_context(|| format!("checksum: invalid extra_files glob: {pattern}"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("checksum: error reading glob results for: {pattern}"))?;
        for m in matches {
            if m.is_file() && !paths.contains(&m) {
                paths.push(m);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

// ---------------------------------------------------------------------------
// ChecksumStage
// ---------------------------------------------------------------------------

pub struct ChecksumStage;

impl Stage for ChecksumStage {
    fn name(&self) -> &str {
        "checksum"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        if ctx.is_dry_run() {
            eprintln!("[checksum] (dry-run) skipping checksum generation");
            return Ok(());
        }

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        // Check global disable flag
        let global_disabled = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref())
            .and_then(|c| c.disable)
            .unwrap_or(false);

        if global_disabled {
            eprintln!("[checksum] globally disabled, skipping");
            return Ok(());
        }

        // Global checksum defaults
        let global_algorithm = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref())
            .and_then(|c| c.algorithm.clone())
            .unwrap_or_else(|| "sha256".to_string());
        let global_name_template = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref())
            .and_then(|c| c.name_template.clone());
        let global_extra_files = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref())
            .and_then(|c| c.extra_files.clone());
        let global_ids = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref())
            .and_then(|c| c.ids.clone());

        // Collect crate configs up-front to avoid borrow conflicts
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for crate_cfg in &crates {
            let crate_name = &crate_cfg.name;

            // Skip crates that have checksum explicitly disabled
            if crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.disable)
                .unwrap_or(false)
            {
                eprintln!("[checksum] disabled for crate {crate_name}, skipping");
                continue;
            }

            // Skip crates that have archives explicitly disabled
            if matches!(crate_cfg.archives, ArchivesConfig::Disabled) {
                eprintln!("[checksum] archives disabled for crate {crate_name}, skipping");
                continue;
            }

            // Per-crate overrides
            let algorithm = crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.algorithm.clone())
                .unwrap_or_else(|| global_algorithm.clone());

            let name_template = crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.name_template.clone())
                .or_else(|| global_name_template.clone());

            let extra_files = crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.extra_files.clone())
                .or_else(|| global_extra_files.clone());

            let ids_filter = crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.ids.clone())
                .or_else(|| global_ids.clone());

            // Gather Archive and LinuxPackage artifacts for this crate
            let mut source_artifacts: Vec<Artifact> = Vec::new();

            let archive_artifacts = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Archive, crate_name)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let package_artifacts = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::LinuxPackage, crate_name)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();

            // Apply ids filter: only include artifacts whose metadata "id" is in the list
            if let Some(ref ids) = ids_filter {
                source_artifacts.extend(archive_artifacts.into_iter().filter(|a| {
                    a.metadata
                        .get("id")
                        .map(|id| ids.contains(id))
                        .unwrap_or(false)
                }));
                source_artifacts.extend(package_artifacts.into_iter().filter(|a| {
                    a.metadata
                        .get("id")
                        .map(|id| ids.contains(id))
                        .unwrap_or(false)
                }));
            } else {
                source_artifacts.extend(archive_artifacts);
                source_artifacts.extend(package_artifacts);
            }

            // Resolve extra_files globs and create synthetic artifacts for them
            if let Some(ref patterns) = extra_files {
                let extra_paths = resolve_extra_files(patterns)?;
                for ep in extra_paths {
                    source_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive, // treated as a checksummable file
                        path: ep,
                        target: None,
                        crate_name: crate_name.clone(),
                        metadata: {
                            let mut m = HashMap::new();
                            m.insert("extra_file".to_string(), "true".to_string());
                            m
                        },
                    });
                }
            }

            if source_artifacts.is_empty() {
                eprintln!(
                    "[checksum] no Archive/LinuxPackage artifacts for crate {crate_name}, skipping"
                );
                continue;
            }

            // Extension for individual sidecar files
            let ext = &algorithm; // e.g. "sha256" or "sha512"

            let mut combined_lines: Vec<String> = Vec::new();

            for artifact in &source_artifacts {
                let hash = hash_file(&artifact.path, &algorithm).with_context(|| {
                    format!(
                        "checksum: hashing {} for crate {crate_name}",
                        artifact.path.display()
                    )
                })?;

                let filename = artifact
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");

                // Build the sidecar path next to the artifact: e.g. myapp.tar.gz.sha256
                let sidecar_path = artifact
                    .path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(format!("{}.{}", filename, ext));

                let line = format_checksum_line(&hash, filename);
                let mut sidecar_file = File::create(&sidecar_path).with_context(|| {
                    format!("checksum: create sidecar {}", sidecar_path.display())
                })?;
                writeln!(sidecar_file, "{}", line).with_context(|| {
                    format!("checksum: write sidecar {}", sidecar_path.display())
                })?;

                eprintln!(
                    "[checksum] {} -> {} ({})",
                    artifact.path.display(),
                    sidecar_path.display(),
                    algorithm
                );

                combined_lines.push(line);

                // Register sidecar as a Checksum artifact
                new_artifacts.push(Artifact {
                    kind: ArtifactKind::Checksum,
                    path: sidecar_path,
                    target: artifact.target.clone(),
                    crate_name: crate_name.clone(),
                    metadata: {
                        let mut m = HashMap::new();
                        m.insert("algorithm".to_string(), algorithm.clone());
                        m.insert(
                            "source".to_string(),
                            artifact.path.to_string_lossy().into_owned(),
                        );
                        m
                    },
                });
            }

            // Write combined checksums file
            let combined_filename = if let Some(tmpl) = &name_template {
                ctx.render_template(tmpl)
                    .with_context(|| format!("checksum: render name_template for {crate_name}"))?
            } else {
                format!("{}_checksums.{}", crate_name, ext)
            };

            let combined_path = dist.join(&combined_filename);
            std::fs::create_dir_all(&dist)
                .with_context(|| format!("checksum: create dist dir {}", dist.display()))?;

            let mut combined_file = File::create(&combined_path).with_context(|| {
                format!("checksum: create combined file {}", combined_path.display())
            })?;
            for line in &combined_lines {
                writeln!(combined_file, "{}", line).with_context(|| {
                    format!("checksum: write combined file {}", combined_path.display())
                })?;
            }

            eprintln!(
                "[checksum] combined checksums -> {}",
                combined_path.display()
            );

            new_artifacts.push(Artifact {
                kind: ArtifactKind::Checksum,
                path: combined_path,
                target: None,
                crate_name: crate_name.clone(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert("algorithm".to_string(), algorithm.clone());
                    m.insert("combined".to_string(), "true".to_string());
                    m
                },
            });
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
        assert!(
            hash.starts_with("fdbd8e75a67f29f701a4e040385e2e23986303ea10239211af907fcbb83578b3")
        );
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
        assert!(
            hash.starts_with("021ced8799296ceca557832ab941a50b4a11f83478cf141f51f933f653ab9fbc")
        );
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

        // Unsupported algorithm should fail
        assert!(hash_file(&f, "md5").is_err());
    }

    #[test]
    fn test_format_checksum_line() {
        let line = format_checksum_line("abcdef1234", "myfile.tar.gz");
        assert_eq!(line, "abcdef1234  myfile.tar.gz");
    }

    // -- Config parsing tests -------------------------------------------------

    #[test]
    fn test_extra_files_config_parsing() {
        let yaml = r#"
name_template: "checksums.txt"
algorithm: "sha256"
extra_files:
  - "dist/*.bin"
  - "README.md"
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.extra_files,
            Some(vec!["dist/*.bin".to_string(), "README.md".to_string()])
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
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.ids,
            Some(vec!["linux-amd64".to_string(), "darwin-arm64".to_string()])
        );
    }

    // -- Stage integration tests ----------------------------------------------

    #[test]
    fn test_checksum_stage_run() {
        use anodize_core::config::CrateConfig;
        use anodize_core::test_helpers::TestContextBuilder;

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
            path: archive_path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // Should have registered Checksum artifacts (sidecar + combined)
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert_eq!(checksums.len(), 2);

        // Sidecar file should exist next to the archive
        let sidecar = dist.join("myapp-1.0.0-linux-amd64.tar.gz.sha256");
        assert!(sidecar.exists(), "sidecar file should exist");
        let sidecar_content = fs::read_to_string(&sidecar).unwrap();
        assert!(sidecar_content.contains("  myapp-1.0.0-linux-amd64.tar.gz"));

        // Combined file should exist in dist
        let combined = dist.join("myapp_checksums.sha256");
        assert!(combined.exists(), "combined checksums file should exist");
        let combined_content = fs::read_to_string(&combined).unwrap();
        assert!(combined_content.contains("  myapp-1.0.0-linux-amd64.tar.gz"));
    }

    #[test]
    fn test_checksum_stage_dry_run() {
        use anodize_core::config::CrateConfig;
        use anodize_core::test_helpers::TestContextBuilder;

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
            path: archive_path.clone(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // In dry-run, no Checksum artifacts are registered
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert!(checksums.is_empty());
    }

    #[test]
    fn test_checksum_stage_sha512() {
        use anodize_core::config::{ChecksumConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
            path: archive_path.clone(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        let sidecar = dist.join("myapp.tar.gz.sha512");
        assert!(sidecar.exists(), "sha512 sidecar should exist");
        let content = fs::read_to_string(&sidecar).unwrap();
        // SHA512 hex is 128 chars
        let hash_part = content.split_whitespace().next().unwrap_or("");
        assert_eq!(hash_part.len(), 128);

        let combined = dist.join("myapp_checksums.sha512");
        assert!(combined.exists());
    }

    #[test]
    fn test_checksum_stage_no_artifacts_skips() {
        use anodize_core::config::CrateConfig;
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::{ChecksumConfig, CrateConfig, Defaults};
        use anodize_core::test_helpers::TestContextBuilder;

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
                    disable: Some(true),
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
            path: archive_path,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // No checksums should be generated when globally disabled
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert!(checksums.is_empty());
    }

    #[test]
    fn test_checksum_stage_per_crate_disable() {
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                disable: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive_path,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // No checksums should be generated for the disabled crate
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert!(checksums.is_empty());
    }

    #[test]
    fn test_checksum_stage_with_extra_files() {
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                extra_files: Some(vec![glob_pattern]),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive_path.clone(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // 3 sidecar files (archive + 2 extra) + 1 combined = 4 artifacts
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert_eq!(checksums.len(), 4);

        // Combined file should include all three files
        let combined = dist.join("myapp_checksums.sha256");
        assert!(combined.exists());
        let content = fs::read_to_string(&combined).unwrap();
        assert!(content.contains("myapp.tar.gz"));
        assert!(content.contains("extra1.bin"));
        assert!(content.contains("extra2.bin"));
    }

    #[test]
    fn test_checksum_stage_with_ids_filter() {
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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

        // Archive with matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive1.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "linux-amd64".to_string());
                m
            },
        });

        // Archive with non-matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive2.clone(),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "darwin-arm64".to_string());
                m
            },
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // Only the linux archive should be checksummed: 1 sidecar + 1 combined = 2
        let checksums = ctx.artifacts.by_kind(ArtifactKind::Checksum);
        assert_eq!(checksums.len(), 2);

        // Combined file should only contain the linux archive
        let combined = dist.join("myapp_checksums.sha256");
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

        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
            path: file1.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: file2.clone(),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // Verify sidecar file format: each line is "<hash>  <filename>\n"
        let sidecar1 = dist.join("app-linux.tar.gz.sha256");
        assert!(sidecar1.exists());
        let sidecar1_content = fs::read_to_string(&sidecar1).unwrap();
        let sidecar1_line = sidecar1_content.trim();

        // Verify format: exactly two spaces between hash and filename
        let parts: Vec<&str> = sidecar1_line.splitn(2, "  ").collect();
        assert_eq!(
            parts.len(),
            2,
            "checksum line should have hash and filename separated by two spaces"
        );
        assert_eq!(
            parts[0],
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(parts[1], "app-linux.tar.gz");

        // Verify second sidecar
        let sidecar2 = dist.join("app-darwin.tar.gz.sha256");
        assert!(sidecar2.exists());
        let sidecar2_content = fs::read_to_string(&sidecar2).unwrap();
        let sidecar2_line = sidecar2_content.trim();
        let parts2: Vec<&str> = sidecar2_line.splitn(2, "  ").collect();
        assert_eq!(
            parts2[0],
            "916f0027a575074ce72a331777c3478d6513f786a591bd892da1a577bf2335f9"
        );
        assert_eq!(parts2[1], "app-darwin.tar.gz");

        // Verify combined checksums file has correct multi-line format
        let combined = dist.join("app_checksums.sha256");
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

        // Verify the combined file contains both filenames
        assert!(combined_content.contains("app-linux.tar.gz"));
        assert!(combined_content.contains("app-darwin.tar.gz"));
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

        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive.clone(),
            target: None,
            crate_name: "fox".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // Independently compute the SHA-256 hash using the crate's own function
        let expected_hash = sha256_file(&archive).unwrap();

        // Read the sidecar and extract the hash
        let sidecar = dist.join("release.tar.gz.sha256");
        let sidecar_content = fs::read_to_string(&sidecar).unwrap();
        let actual_hash = sidecar_content.trim().split("  ").next().unwrap();

        assert_eq!(
            actual_hash, expected_hash,
            "sidecar hash should match independently computed hash"
        );

        // Also verify via the combined file
        let combined = dist.join("fox_checksums.sha256");
        let combined_content = fs::read_to_string(&combined).unwrap();
        let combined_hash = combined_content.trim().split("  ").next().unwrap();
        assert_eq!(
            combined_hash, expected_hash,
            "combined file hash should match too"
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

        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive.clone(),
            target: None,
            crate_name: "pkg".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        let sidecar = dist.join("pkg.tar.gz.sha512");
        assert!(sidecar.exists());
        let content = fs::read_to_string(&sidecar).unwrap();
        let hash = content.trim().split("  ").next().unwrap();
        assert_eq!(hash.len(), 128, "SHA-512 should produce 128 hex chars");

        // Independently verify the hash value
        let expected = sha512_file(&archive).unwrap();
        assert_eq!(hash, expected);
    }

    // -- TestContextBuilder + create_fake_binary integration test --

    #[test]
    fn test_checksum_of_fake_binary_via_builder() {
        use anodize_core::test_helpers::{TestContextBuilder, create_fake_binary};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();

        let fake_bin = create_fake_binary(&dist, "myapp-linux.tar.gz");

        let mut ctx = TestContextBuilder::new()
            .project_name("checksum-test")
            .tag("v2.0.0")
            .dist(dist.clone())
            .crates(vec![anodize_core::config::CrateConfig {
                name: "checksum-test".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: fake_bin.clone(),
            target: None,
            crate_name: "checksum-test".to_string(),
            metadata: Default::default(),
        });

        let stage = ChecksumStage;
        stage.run(&mut ctx).unwrap();

        // Verify sidecar was created with correct hash
        let sidecar = dist.join("myapp-linux.tar.gz.sha256");
        assert!(sidecar.exists(), "sidecar should be created for fake binary");
        let sidecar_content = fs::read_to_string(&sidecar).unwrap();
        let expected_hash = sha256_file(&fake_bin).unwrap();
        assert!(sidecar_content.starts_with(&expected_hash));
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
        assert!(hash_file(&f, "sha512")
            .unwrap()
            .starts_with("309ecc489c12d6eb4cc40f50c902f2b4"));
    }

    #[test]
    fn test_checksum_file_registered_as_checksum_artifact() {
        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        ChecksumStage.run(&mut ctx).unwrap();

        // All checksum artifacts should have kind = Checksum
        for a in ctx.artifacts.by_kind(ArtifactKind::Checksum) {
            assert_eq!(a.kind, ArtifactKind::Checksum);
            assert!(a.metadata.contains_key("algorithm"));
        }

        // Combined file should have "combined" metadata
        let combined = ctx
            .artifacts
            .by_kind(ArtifactKind::Checksum)
            .into_iter()
            .find(|a| a.metadata.get("combined") == Some(&"true".to_string()));
        assert!(combined.is_some(), "should have a combined checksum artifact");
    }

    #[test]
    fn test_checksum_missing_file_errors() {
        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
            path: nonexistent,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
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
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                    extra_files: Some(vec![glob_pattern]),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: archive,
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
        });

        ChecksumStage.run(&mut ctx).unwrap();

        // Combined file should include both archive and extra file
        let combined = dist.join("app_checksums.sha256");
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

    #[test]
    fn test_ids_filter_excludes_unmatched_artifacts() {
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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

        // Add 3 artifacts, only 2 have matching ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: linux,
            target: None,
            crate_name: "app".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "linux".to_string());
                m
            },
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: darwin,
            target: None,
            crate_name: "app".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "darwin".to_string());
                m
            },
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: windows,
            target: None,
            crate_name: "app".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), "windows".to_string());
                m
            },
        });

        ChecksumStage.run(&mut ctx).unwrap();

        // Combined file should include only linux and darwin
        let combined = dist.join("app_checksums.sha256");
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

        let result = hash_file(&f, "md5");
        assert!(result.is_err(), "unsupported algorithm should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unsupported checksum algorithm") && err.contains("md5"),
            "error should mention 'unsupported checksum algorithm' and 'md5', got: {err}"
        );
    }

    #[test]
    fn test_unsupported_algorithm_crc32() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("test.txt");
        fs::write(&f, b"hello").unwrap();

        let result = hash_file(&f, "crc32");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unsupported") && err.contains("crc32"),
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
            err.contains("No such file or directory") || err.contains("not found"),
            "error should mention missing directory, got: {err}"
        );
    }

    #[test]
    fn test_each_sha_algorithm_on_missing_file() {
        let missing = Path::new("/nonexistent/checksum_test_file");
        for algo in &["sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s"] {
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
}
