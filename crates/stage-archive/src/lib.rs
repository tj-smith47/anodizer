use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use flate2::Compression;
use flate2::write::GzEncoder;

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
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
pub fn create_tar_gz(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.gz: {}", output.display()))?;
    let enc = GzEncoder::new(out_file, Compression::default());
    let mut tar = tar::Builder::new(enc);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let archive_name = compute_archive_name(src, base_dir, wrap_dir);
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
// create_tar_xz
// ---------------------------------------------------------------------------

/// Create a tar.xz archive containing the given files.
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
pub fn create_tar_xz(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.xz: {}", output.display()))?;
    let enc = xz2::write::XzEncoder::new(out_file, 6);
    let mut tar = tar::Builder::new(enc);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let archive_name = compute_archive_name(src, base_dir, wrap_dir);
        tar.append_path_with_name(src, &archive_name)
            .with_context(|| {
                format!(
                    "tar.xz: adding {} as {}",
                    src.display(),
                    archive_name.display()
                )
            })?;
    }

    tar.finish().context("tar.xz: finish")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// create_tar_zst
// ---------------------------------------------------------------------------

/// Create a tar.zst archive containing the given files.
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
pub fn create_tar_zst(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.zst: {}", output.display()))?;
    let enc = zstd::Encoder::new(out_file, 3).context("tar.zst: create zstd encoder")?;
    let mut tar = tar::Builder::new(enc);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let archive_name = compute_archive_name(src, base_dir, wrap_dir);
        tar.append_path_with_name(src, &archive_name)
            .with_context(|| {
                format!(
                    "tar.zst: adding {} as {}",
                    src.display(),
                    archive_name.display()
                )
            })?;
    }

    let enc = tar.into_inner().context("tar.zst: finish tar")?;
    enc.finish().context("tar.zst: finish zstd")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// create_zip
// ---------------------------------------------------------------------------

/// Create a zip archive containing the given files.
/// Each file is stored under its own filename (no directory prefix).
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
pub fn create_zip(files: &[&Path], output: &Path, wrap_dir: Option<&str>) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create zip: {}", output.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for &src in files {
        if !src.exists() {
            continue;
        }
        let base_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let name = if let Some(dir) = wrap_dir {
            format!("{dir}/{base_name}")
        } else {
            base_name.to_string()
        };
        zip.start_file(&name, options)
            .with_context(|| format!("zip: start_file {name}"))?;
        let data = fs::read(src).with_context(|| format!("zip: read {}", src.display()))?;
        zip.write_all(&data)
            .with_context(|| format!("zip: write {name}"))?;
    }

    zip.finish().context("zip: finish")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// copy_binary
// ---------------------------------------------------------------------------

/// Copy binary files directly to the output directory (no archiving).
/// For a single file, copies to `output` directly.
/// For multiple files, copies each file into the parent directory of `output`.
pub fn copy_binary(files: &[&Path], output: &Path) -> Result<()> {
    if files.len() == 1 {
        let src = files[0];
        if !src.exists() {
            anyhow::bail!("binary: source does not exist: {}", src.display());
        }
        fs::copy(src, output)
            .with_context(|| format!("binary: copy {} -> {}", src.display(), output.display()))?;
    } else {
        let out_dir = output.parent().unwrap_or(Path::new("."));
        for &src in files {
            if !src.exists() {
                continue;
            }
            let file_name = src.file_name().unwrap_or(src.as_os_str());
            let dest = out_dir.join(file_name);
            fs::copy(src, &dest)
                .with_context(|| format!("binary: copy {} -> {}", src.display(), dest.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// resolve_glob_patterns
// ---------------------------------------------------------------------------

/// Resolve a list of file patterns, expanding glob patterns.
/// Non-glob entries are treated as literal paths.
pub fn resolve_glob_patterns(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    for pattern in patterns {
        // Check if the pattern contains glob metacharacters
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let entries =
                glob::glob(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?;
            for entry in entries {
                let path = entry.with_context(|| format!("glob error for pattern: {pattern}"))?;
                results.push(path);
            }
        } else {
            let p = PathBuf::from(pattern);
            if p.exists() {
                results.push(p);
            }
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// compute_archive_name  (helper)
// ---------------------------------------------------------------------------

/// Compute the archive entry name for a source file.
/// If `base_dir` is provided, the source is stored relative to it.
/// If `wrap_dir` is provided, the entry is prefixed with that directory name.
fn compute_archive_name(src: &Path, base_dir: Option<&Path>, wrap_dir: Option<&str>) -> PathBuf {
    let relative = if let Some(base) = base_dir {
        src.strip_prefix(base)
            .unwrap_or_else(|_| src.file_name().map(Path::new).unwrap_or(src))
            .to_path_buf()
    } else {
        PathBuf::from(src.file_name().unwrap_or(src.as_os_str()))
    };

    if let Some(dir) = wrap_dir {
        PathBuf::from(dir).join(relative)
    } else {
        relative
    }
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
                let name_tmpl = archive_cfg.name_template.as_deref().unwrap_or(default_tmpl);

                for (target, target_bins) in &by_target {
                    // Filter binaries for this archive config
                    let selected_bins: Vec<&Artifact> = target_bins
                        .iter()
                        .filter(|b| match binary_filter {
                            None => true,
                            Some(names) => {
                                let bin_name =
                                    b.metadata.get("binary").map(|s| s.as_str()).unwrap_or("");
                                names.iter().any(|n| n == bin_name)
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

                    // Set Binary to the first selected binary's name (matches GoReleaser behavior)
                    if let Some(bin_name) =
                        selected_bins.first().and_then(|b| b.metadata.get("binary"))
                    {
                        tvars.set("Binary", bin_name);
                    }

                    // Render wrap_in_directory (template-aware)
                    let wrap_dir_rendered =
                        if let Some(tmpl) = archive_cfg.wrap_in_directory.as_deref() {
                            Some(ctx.render_template(tmpl).with_context(|| {
                                format!("render wrap_in_directory for {crate_name}/{target}")
                            })?)
                        } else {
                            None
                        };
                    let wrap_dir = wrap_dir_rendered.as_deref();

                    // Render name
                    let archive_stem = ctx.render_template(name_tmpl).with_context(|| {
                        format!("render archive name for {crate_name}/{target}")
                    })?;

                    // For binary format, no extension; otherwise append format
                    let archive_filename = if format == "binary" {
                        archive_stem.clone()
                    } else {
                        format!("{archive_stem}.{format}")
                    };
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

                    // Extra files (LICENSE, README, etc.) — with glob support
                    if let Some(extra_files) = &archive_cfg.files {
                        let resolved = resolve_glob_patterns(extra_files).with_context(|| {
                            format!("resolve file patterns for {crate_name}/{target}")
                        })?;
                        paths.extend(resolved);
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
                            "zip" => create_zip(&path_refs, &archive_path, wrap_dir)?,
                            "tar.xz" => create_tar_xz(&path_refs, &archive_path, None, wrap_dir)?,
                            "tar.zst" => create_tar_zst(&path_refs, &archive_path, None, wrap_dir)?,
                            "binary" => copy_binary(&path_refs, &archive_path)?,
                            _ => create_tar_gz(&path_refs, &archive_path, None, wrap_dir)?,
                        }
                    }

                    // Update stage-scoped template vars for downstream stages
                    let tvars = ctx.template_vars_mut();
                    tvars.set("ArtifactName", &archive_filename);
                    tvars.set("ArtifactPath", &archive_path.to_string_lossy());

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
#[allow(clippy::field_reassign_with_default)]
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
        create_tar_gz(&[&bin_path], &archive_path, None, None).unwrap();

        assert!(archive_path.exists());
        assert!(fs::metadata(&archive_path).unwrap().len() > 0);
    }

    #[test]
    fn test_create_zip() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin.exe");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin.zip");
        create_zip(&[&bin_path], &archive_path, None).unwrap();

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
    // New tests: tar.xz
    // ---------------------------------------------------------------------------

    #[test]
    fn test_create_tar_xz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content for xz").unwrap();

        let archive_path = tmp.path().join("mybin.tar.xz");
        create_tar_xz(&[&bin_path], &archive_path, None, None).unwrap();

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
        create_tar_zst(&[&bin_path], &archive_path, None, None).unwrap();

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
        create_zip(&[&bin_path], &archive_path, Some("myapp-1.0.0")).unwrap();

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
        create_tar_xz(&[&bin_path], &archive_path, None, Some("myapp-1.0.0")).unwrap();

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
        create_tar_zst(&[&bin_path], &archive_path, None, Some("myapp-1.0.0")).unwrap();

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
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - format: tar.gz
        wrap_in_directory: "myapp-{{ .Version }}"
        files:
          - LICENSE
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        match &config.crates[0].archives {
            ArchivesConfig::Configs(cfgs) => {
                assert_eq!(cfgs.len(), 1);
                assert_eq!(
                    cfgs[0].wrap_in_directory,
                    Some("myapp-{{ .Version }}".to_string())
                );
                assert_eq!(cfgs[0].format, Some("tar.gz".to_string()));
            }
            _ => panic!("expected Configs variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Integration-style test: ArchiveStage.run
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_run() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
                    format: Some("tar.gz".to_string()),
                    format_overrides: None,
                    files: None,
                    binaries: None,
                    wrap_in_directory: None,
                }]),
                ..Default::default()
            }])
            .build();

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
        use anodize_core::config::{ArchivesConfig, CrateConfig};
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
                archives: ArchivesConfig::Disabled,
                ..Default::default()
            }])
            .build();

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
        use anodize_core::config::{
            ArchiveConfig, ArchivesConfig, Config, CrateConfig, FormatOverride,
        };
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
                wrap_in_directory: None,
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

    // ---------------------------------------------------------------------------
    // Integration test: ArchiveStage with tar.xz format
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_tar_xz_format() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.xz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
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
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("binary".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
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

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        // Binary format should have no extension
        let name = archives[0].path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains(".tar"));
        assert!(!name.contains(".zip"));
        assert!(!name.contains(".gz"));
        assert!(archives[0].path.exists());
        assert_eq!(fs::read(&archives[0].path).unwrap(), b"raw binary content");
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
        create_tar_gz(&[&bin, &license, &readme], &archive_path, None, None).unwrap();

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
        create_zip(&[&bin, &license, &readme], &archive_path, None).unwrap();

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
        create_tar_xz(&[&bin, &license, &readme], &archive_path, None, None).unwrap();

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
        create_tar_zst(&[&bin, &license, &readme], &archive_path, None, None).unwrap();

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
        use anodize_core::test_helpers::TestContextBuilder;

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
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_for_target_multiple_overrides() {
        // Multiple OS overrides: windows->zip AND darwin->tar.gz
        let overrides = vec![
            FormatOverride {
                os: "windows".to_string(),
                format: "zip".to_string(),
            },
            FormatOverride {
                os: "darwin".to_string(),
                format: "tar.gz".to_string(),
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
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None, // Include all binaries
                wrap_in_directory: None,
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
            path: bin1.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: bin2.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myhelper".to_string());
                m
            },
        });

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
        assert_eq!(found_files.len(), 2, "archive should contain both binaries");
        assert!(found_files.contains_key("myapp"));
        assert!(found_files.contains_key("myhelper"));
    }

    #[test]
    fn test_archive_stage_default_config_inheritance() {
        use anodize_core::config::{
            ArchiveConfig, ArchivesConfig, Config, CrateConfig, DefaultArchiveConfig, Defaults,
            FormatOverride,
        };
        use anodize_core::context::{Context, ContextOptions};

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
            archives: Some(DefaultArchiveConfig {
                format: Some("tar.gz".to_string()),
                format_overrides: Some(vec![FormatOverride {
                    os: "windows".to_string(),
                    format: "zip".to_string(),
                }]),
            }),
            ..Default::default()
        });

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: bin.clone(),
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
        // Should have inherited global format_override: windows -> zip
        assert!(
            archives[0].path.to_string_lossy().ends_with(".zip"),
            "windows archive should use zip from global defaults, got: {}",
            archives[0].path.display()
        );
    }

    #[test]
    fn test_archive_stage_name_template_renders_all_variables() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
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
            path: bin.clone(),
            target: Some("aarch64-apple-darwin".to_string()),
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

        let name = archives[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "myapp_2.5.0_darwin_arm64.tar.gz");
    }

    #[test]
    fn test_archive_stage_files_included_alongside_binaries() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: Some(vec![license_pattern, readme_pattern]),
                binaries: None,
                wrap_in_directory: None,
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
            path: bin.clone(),
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
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("zip".to_string()),
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
            path: bin,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        // Verify the artifact metadata contains format and name
        assert_eq!(
            archives[0].metadata.get("format"),
            Some(&"zip".to_string())
        );
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
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                format: Some("tar.gz".to_string()),
                wrap_in_directory: Some("{{ .ProjectName }}-{{ .Version }}".to_string()),
                files: None,
                format_overrides: None,
                binaries: None,
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
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
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

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_binary_artifact_errors_with_path() {
        use anodize_core::config::{ArchiveConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: anodize_core::config::ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
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
            path: PathBuf::from("/nonexistent/path/to/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
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
        let result = create_tar_gz(&[], &archive_path, None, None);
        assert!(result.is_ok(), "creating archive with empty file list should succeed");
        assert!(archive_path.exists(), "archive file should be created");
    }

    #[test]
    fn test_empty_file_list_creates_empty_zip() {
        let tmp = TempDir::new().unwrap();
        let archive_path = tmp.path().join("empty.zip");

        let result = create_zip(&[], &archive_path, None);
        assert!(result.is_ok(), "creating zip with empty file list should succeed");
        assert!(archive_path.exists(), "zip file should be created");
    }

    #[test]
    fn test_copy_binary_source_missing_errors_with_path() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let output = tmp.path().join("output");

        let result = copy_binary(&[missing.as_path()], &output);
        assert!(result.is_err(), "copy_binary with missing source should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist") || err.contains("does-not-exist"),
            "error should mention the missing file, got: {err}"
        );
    }

    #[test]
    fn test_archive_unsupported_format_falls_back_to_tar_gz() {
        // The archive stage treats unknown formats as tar.gz (the default branch)
        // This tests that an unusual format string doesn't crash but falls back.
        use anodize_core::config::{ArchiveConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
            archives: anodize_core::config::ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("unsupported_format".to_string()),
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
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("binary".to_string(), "mybin".to_string());
                m
            },
        });

        // Should succeed because unknown format falls back to tar.gz
        let result = ArchiveStage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "unknown format should fall back to tar.gz, got: {:?}",
            result.err()
        );
    }
}
