use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use flate2::write::GzEncoder;
use flate2::Compression;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{ArchiveConfig, ArchivesConfig, FormatOverride};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

// ---------------------------------------------------------------------------
// create_tar_gz
// ---------------------------------------------------------------------------

/// Create a tar.gz archive containing the given files.
/// Each file is stored under its own filename (no directory prefix) unless
/// `base_dir` is provided, in which case files are stored relative to it.
pub fn create_tar_gz(files: &[&Path], output: &Path, base_dir: Option<&Path>) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.gz: {}", output.display()))?;
    let enc = GzEncoder::new(out_file, Compression::default());
    let mut tar = tar::Builder::new(enc);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let archive_name = if let Some(base) = base_dir {
            src.strip_prefix(base)
                .unwrap_or_else(|_| src.file_name().map(Path::new).unwrap_or(src))
                .to_path_buf()
        } else {
            PathBuf::from(src.file_name().unwrap_or(src.as_os_str()))
        };
        tar.append_path_with_name(src, &archive_name)
            .with_context(|| {
                format!(
                    "tar.gz: adding {} as {}",
                    src.display(),
                    archive_name.display()
                )
            })?;
    }

    tar.finish().context("tar.gz: finish")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// create_zip
// ---------------------------------------------------------------------------

/// Create a zip archive containing the given files.
/// Each file is stored under its own filename (no directory prefix).
pub fn create_zip(files: &[&Path], output: &Path) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create zip: {}", output.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        zip.start_file(name, options)
            .with_context(|| format!("zip: start_file {name}"))?;
        let data = fs::read(src)
            .with_context(|| format!("zip: read {}", src.display()))?;
        zip.write_all(&data)
            .with_context(|| format!("zip: write {name}"))?;
    }

    zip.finish().context("zip: finish")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// format_for_target
// ---------------------------------------------------------------------------

/// Determine the archive format for a target, applying OS-based overrides.
pub fn format_for_target(
    target: &str,
    default_format: &str,
    overrides: &[FormatOverride],
) -> String {
    let (os, _arch) = map_target(target);
    for ov in overrides {
        if ov.os == os {
            return ov.format.clone();
        }
    }
    default_format.to_string()
}

// ---------------------------------------------------------------------------
// default_name_template
// ---------------------------------------------------------------------------

fn default_name_template() -> &'static str {
    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
}

// ---------------------------------------------------------------------------
// ArchiveStage
// ---------------------------------------------------------------------------

pub struct ArchiveStage;

impl Stage for ArchiveStage {
    fn name(&self) -> &str {
        "archive"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        // Global archive defaults
        let global_default_format = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.format.clone())
            .unwrap_or_else(|| "tar.gz".to_string());
        let global_format_overrides: Vec<FormatOverride> = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.format_overrides.clone())
            .unwrap_or_default();

        // Collect crate configs to avoid borrow conflict later
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Build a list of (crate_name, archive_configs) pairs to process
        let work: Vec<(String, Vec<ArchiveConfig>)> = crates
            .into_iter()
            .filter_map(|c| {
                match &c.archives {
                    ArchivesConfig::Disabled => None,
                    ArchivesConfig::Configs(cfgs) => {
                        let archive_cfgs = if cfgs.is_empty() {
                            // Default: one archive with all defaults
                            vec![ArchiveConfig::default()]
                        } else {
                            cfgs.clone()
                        };
                        Some((c.name.clone(), archive_cfgs))
                    }
                }
            })
            .collect();

        // Ensure dist directory exists
        fs::create_dir_all(&dist)
            .with_context(|| format!("create dist dir: {}", dist.display()))?;

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for (crate_name, archive_cfgs) in &work {
            // Collect Binary artifacts for this crate
            let binaries: Vec<Artifact> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, crate_name)
                .into_iter()
                .cloned()
                .collect();

            if binaries.is_empty() {
                eprintln!("[archive] no binaries for crate {crate_name}, skipping");
                continue;
            }

            // Group binaries by target
            let mut by_target: HashMap<String, Vec<Artifact>> = HashMap::new();
            for bin in binaries {
                let target = bin.target.clone().unwrap_or_else(|| "unknown".to_string());
                by_target.entry(target).or_default().push(bin);
            }

            for archive_cfg in archive_cfgs {
                // Determine format (per-config > global default)
                let default_format = archive_cfg
                    .format
                    .as_deref()
                    .unwrap_or(&global_default_format);

                // Determine format overrides (per-config > global)
                let format_overrides: Vec<FormatOverride> = archive_cfg
                    .format_overrides
                    .clone()
                    .unwrap_or_else(|| global_format_overrides.clone());

                // Determine which binaries to include
                let binary_filter: Option<&Vec<String>> = archive_cfg.binaries.as_ref();

                // Name template
                let default_tmpl = default_name_template();
                let name_tmpl = archive_cfg
                    .name_template
                    .as_deref()
                    .unwrap_or(default_tmpl);

                for (target, target_bins) in &by_target {
                    // Filter binaries for this archive config
                    let selected_bins: Vec<&Artifact> = target_bins
                        .iter()
                        .filter(|b| {
                            match binary_filter {
                                None => true,
                                Some(names) => {
                                    let bin_name = b
                                        .metadata
                                        .get("binary")
                                        .map(|s| s.as_str())
                                        .unwrap_or("");
                                    names.iter().any(|n| n == bin_name)
                                }
                            }
                        })
                        .collect();

                    if selected_bins.is_empty() {
                        continue;
                    }

                    // Resolve archive format for this target
                    let format = format_for_target(target, default_format, &format_overrides);
                    let (os, arch) = map_target(target);

                    // Build template vars for this target
                    let tvars = ctx.template_vars_mut();
                    tvars.set("Os", &os);
                    tvars.set("Arch", &arch);

                    // Render name
                    let archive_stem = ctx
                        .render_template(name_tmpl)
                        .with_context(|| format!("render archive name for {crate_name}/{target}"))?;

                    let archive_filename = format!("{archive_stem}.{format}");
                    let archive_path = dist.join(&archive_filename);

                    // Collect binary files — missing binaries are errors
                    let mut paths: Vec<PathBuf> = Vec::new();
                    for b in &selected_bins {
                        if !b.path.exists() && !ctx.options.dry_run {
                            anyhow::bail!(
                                "binary artifact missing: {} (expected at {})",
                                b.metadata.get("binary").unwrap_or(&b.crate_name),
                                b.path.display()
                            );
                        }
                        paths.push(b.path.clone());
                    }

                    // Extra files (LICENSE, README, etc.)
                    if let Some(extra_files) = &archive_cfg.files {
                        for pattern in extra_files {
                            let p = PathBuf::from(pattern);
                            if p.exists() {
                                paths.push(p);
                            }
                        }
                    }

                    let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();

                    if ctx.options.dry_run {
                        eprintln!(
                            "[archive] (dry-run) would create {} with {} files",
                            archive_path.display(),
                            path_refs.len()
                        );
                    } else {
                        eprintln!("[archive] creating {}", archive_path.display());
                        match format.as_str() {
                            "zip" => create_zip(&path_refs, &archive_path)?,
                            _ => create_tar_gz(&path_refs, &archive_path, None)?,
                        }
                    }

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        path: archive_path,
                        target: Some(target.clone()),
                        crate_name: crate_name.clone(),
                        metadata: {
                            let mut m = HashMap::new();
                            m.insert("format".to_string(), format.clone());
                            m.insert("name".to_string(), archive_stem.clone());
                            m
                        },
                    });
                }
            }
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
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use anodize_core::artifact::{Artifact, ArtifactKind};

    #[test]
    fn test_create_tar_gz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin.tar.gz");
        create_tar_gz(&[&bin_path], &archive_path, None).unwrap();

        assert!(archive_path.exists());
        assert!(fs::metadata(&archive_path).unwrap().len() > 0);
    }

    #[test]
    fn test_create_zip() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin.exe");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin.zip");
        create_zip(&[&bin_path], &archive_path).unwrap();

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
                    format: "zip".to_string()
                }]
            ),
            "zip"
        );
    }

    // ---------------------------------------------------------------------------
    // Integration-style test: ArchiveStage.run
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_run() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create a fake binary
        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"fake binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
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

        // Register a Binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: bin_path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        // Should have registered one Archive artifact
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.exists());
        assert!(archives[0].path.to_string_lossy().ends_with(".tar.gz"));
    }

    #[test]
    fn test_archive_stage_disabled() {
        use anodize_core::config::{ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Disabled,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Register a Binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from("/fake/path"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        // No archives should be registered
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert!(archives.is_empty());
    }

    #[test]
    fn test_archive_stage_zip_for_windows() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig, FormatOverride};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.gz".to_string()),
                format_overrides: Some(vec![FormatOverride {
                    os: "windows".to_string(),
                    format: "zip".to_string(),
                }]),
                files: None,
                binaries: None,
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
            path: bin_path.clone(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.to_string_lossy().ends_with(".zip"));
        assert!(archives[0].path.exists());
    }
}
