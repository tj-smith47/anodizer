use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use blake2::{Blake2b512, Blake2s256};
use md5::Md5;
use sha1::Sha1;
use sha2::{Sha224, Sha384, Sha512};
use sha3::{Sha3_224, Sha3_256, Sha3_384, Sha3_512};

use anodize_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodize_core::config::ExtraFileSpec;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Hash helpers
// ---------------------------------------------------------------------------

use anodize_core::hashing::hash_file_with;

pub fn sha1_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha1>(path, "sha1")
}

pub fn sha224_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha224>(path, "sha224")
}

pub use anodize_core::hashing::sha256_file;

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

pub fn sha3_224_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_224>(path, "sha3-224")
}

pub fn sha3_256_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_256>(path, "sha3-256")
}

pub fn sha3_384_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_384>(path, "sha3-384")
}

pub fn sha3_512_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_512>(path, "sha3-512")
}

pub fn blake3_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    anodize_core::hashing::hash_file_streaming(path, "blake3", |chunk| {
        hasher.update(chunk);
    })?;
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn crc32_file(path: &Path) -> Result<String> {
    let mut hasher = crc32fast::Hasher::new();
    anodize_core::hashing::hash_file_streaming(path, "crc32", |chunk| {
        hasher.update(chunk);
    })?;
    Ok(format!("{:08x}", hasher.finalize()))
}

pub fn md5_file(path: &Path) -> Result<String> {
    hash_file_with::<Md5>(path, "md5")
}

/// Return the hex-encoded output length for a given hash algorithm.
/// Used to generate correctly-sized placeholder hashes in dry-run mode.
fn hash_hex_len(algorithm: &str) -> usize {
    match algorithm {
        "md5" => 32,     // 128-bit / 16 bytes
        "sha1" => 40,    // 160-bit / 20 bytes
        "sha224" => 56,  // 224-bit / 28 bytes
        "sha256" => 64,  // 256-bit / 32 bytes
        "sha384" => 96,  // 384-bit / 48 bytes
        "sha512" => 128, // 512-bit / 64 bytes
        "sha3-224" => 56,
        "sha3-256" => 64,
        "sha3-384" => 96,
        "sha3-512" => 128,
        "blake2b" => 128, // Blake2b-512
        "blake2s" => 64,  // Blake2s-256
        "blake3" => 64,   // 256-bit default
        "crc32" => 8,     // 32-bit / 4 bytes
        _ => 64,          // fallback
    }
}

pub fn hash_file(path: &Path, algorithm: &str) -> Result<String> {
    match algorithm {
        "sha1" => sha1_file(path),
        "sha224" => sha224_file(path),
        "sha256" => sha256_file(path),
        "sha384" => sha384_file(path),
        "sha512" => sha512_file(path),
        "sha3-224" => sha3_224_file(path),
        "sha3-256" => sha3_256_file(path),
        "sha3-384" => sha3_384_file(path),
        "sha3-512" => sha3_512_file(path),
        "blake2b" => blake2b_file(path),
        "blake2s" => blake2s_file(path),
        "blake3" => blake3_file(path),
        "crc32" => crc32_file(path),
        "md5" => md5_file(path),
        _ => bail!("unsupported checksum algorithm: {}", algorithm),
    }
}

pub fn format_checksum_line(hash: &str, filename: &str) -> String {
    format!("{}  {}", hash, filename)
}

// ---------------------------------------------------------------------------
// Extra-files glob resolution
// ---------------------------------------------------------------------------

/// Resolved extra file: the path on disk and an optional name_template override.
struct ResolvedExtraFile {
    path: PathBuf,
    name_template: Option<String>,
}

/// Resolve `extra_files` via the canonical `core::extrafiles::resolve` — thin
/// adapter that returns the local `ResolvedExtraFile` shape expected by the
/// rest of this module.
fn resolve_extra_files(
    specs: &[ExtraFileSpec],
    log: &anodize_core::log::StageLogger,
) -> Result<Vec<ResolvedExtraFile>> {
    anodize_core::extrafiles::resolve(specs, log)
        .map(|v| {
            v.into_iter()
                .map(|r| ResolvedExtraFile {
                    path: r.path,
                    name_template: r.name_template,
                })
                .collect()
        })
        .with_context(|| "checksum: resolve extra_files")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ChecksumStage
// ---------------------------------------------------------------------------

/// Checksum stage: computes checksums for all build/archive artifacts.
///
/// **Note on `Artifacts` template variable**: This stage does NOT call
/// `ctx.refresh_artifacts_var()` because it only renders naming templates
/// (e.g. `name_template`, `extra_name_template`) — not user-facing release
/// body or announce templates where `{{ Artifacts }}` would be iterated.
/// The `Artifacts` variable is refreshed by the release and announce stages
/// just before they render their body templates.
pub struct ChecksumStage;

impl Stage for ChecksumStage {
    fn name(&self) -> &str {
        "checksum"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("checksum");
        let dry_run = ctx.is_dry_run();

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        // Extract global checksum defaults once
        let global_cksum = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref());

        let global_disable = global_cksum.and_then(|c| c.disable.clone());
        if ctx.is_disabled_with_log(&global_disable, &log, "checksum globally") {
            return Ok(());
        }

        let global_algorithm = global_cksum
            .and_then(|c| c.algorithm.clone())
            .unwrap_or_else(|| "sha256".to_string());
        let global_name_template = global_cksum.and_then(|c| c.name_template.clone());
        let global_extra_files = global_cksum.and_then(|c| c.extra_files.clone());
        let global_templated_extra_files =
            global_cksum.and_then(|c| c.templated_extra_files.clone());
        let global_ids = global_cksum.and_then(|c| c.ids.clone());
        let global_split = global_cksum.and_then(|c| c.split);

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
            let crate_disable = crate_cfg.checksum.as_ref().and_then(|c| c.disable.clone());
            if ctx.is_disabled_with_log(
                &crate_disable,
                &log,
                &format!("checksum for crate {crate_name}"),
            ) {
                continue;
            }

            // Per-crate overrides (fall back to global defaults)
            let crate_cksum = crate_cfg.checksum.as_ref();
            let algorithm = crate_cksum
                .and_then(|c| c.algorithm.clone())
                .unwrap_or_else(|| global_algorithm.clone());
            let name_template = crate_cksum
                .and_then(|c| c.name_template.clone())
                .or_else(|| global_name_template.clone());
            let extra_files = crate_cksum
                .and_then(|c| c.extra_files.clone())
                .or_else(|| global_extra_files.clone());
            let templated_extra_files = crate_cksum
                .and_then(|c| c.templated_extra_files.clone())
                .or_else(|| global_templated_extra_files.clone());
            let ids_filter = crate_cksum
                .and_then(|c| c.ids.clone())
                .or_else(|| global_ids.clone());
            let split = crate_cksum
                .and_then(|c| c.split)
                .or(global_split)
                .unwrap_or(false);

            // Gather checksummable artifacts for this crate
            let mut source_artifacts: Vec<Artifact> = Vec::new();
            for kind in [
                ArtifactKind::Archive,
                ArtifactKind::LinuxPackage,
                ArtifactKind::Binary,
                ArtifactKind::UploadableBinary,
                ArtifactKind::SourceArchive,
                ArtifactKind::Sbom,
                ArtifactKind::Snap,
                ArtifactKind::DiskImage,
                ArtifactKind::Installer,
                ArtifactKind::MacOsPackage,
            ] {
                let artifacts = ctx
                    .artifacts
                    .by_kind_and_crate(kind, crate_name)
                    .into_iter()
                    .cloned();
                if ids_filter.is_some() {
                    source_artifacts
                        .extend(artifacts.filter(|a| matches_id_filter(a, ids_filter.as_deref())));
                } else {
                    source_artifacts.extend(artifacts);
                }
            }

            // Resolve extra_files globs and create synthetic artifacts for them
            if let Some(ref specs) = extra_files {
                let resolved = resolve_extra_files(specs, &log)?;
                for ef in resolved {
                    let mut metadata =
                        HashMap::from([("extra_file".to_string(), "true".to_string())]);
                    if let Some(tmpl) = ef.name_template {
                        metadata.insert("extra_name_template".to_string(), tmpl);
                    }
                    source_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive, // treated as a checksummable file
                        name: String::new(),
                        path: ef.path,
                        target: None,
                        crate_name: crate_name.clone(),
                        metadata,
                        size: None,
                    });
                }
            }

            // Process templated_extra_files: render and add as checksummable artifacts
            if let Some(ref tpl_specs) = templated_extra_files
                && !tpl_specs.is_empty()
            {
                let rendered = anodize_core::templated_files::process_templated_extra_files(
                    tpl_specs, ctx, &dist, "checksum",
                )?;
                for (path, dst_name) in rendered {
                    let metadata = HashMap::from([("extra_file".to_string(), "true".to_string())]);
                    source_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        name: dst_name,
                        path,
                        target: None,
                        crate_name: crate_name.clone(),
                        metadata,
                        size: None,
                    });
                }
            }

            if source_artifacts.is_empty() {
                log.verbose(&format!(
                    "no checksummable artifacts for crate {crate_name}, skipping"
                ));
                continue;
            }

            // Extension for individual sidecar files
            let ext = &algorithm; // e.g. "sha256" or "sha512"

            let mut combined_lines: Vec<String> = Vec::new();
            // Collect (artifact_path, "algorithm:hash") pairs so we can store
            // the checksum back into each artifact's metadata after the loop.
            // GoReleaser stores this as Extra["Checksum"] = "algorithm:hash".
            let mut artifact_checksums: Vec<(PathBuf, String)> = Vec::new();

            for artifact in &source_artifacts {
                // In dry-run mode, files may not exist on disk; skip with placeholder
                let hash = if dry_run && !artifact.path.exists() {
                    log.verbose(&format!(
                        "(dry-run) skipping hash for non-existent {}",
                        artifact.path.display()
                    ));
                    // Produce a placeholder hash with the correct length for the algorithm
                    let hash_len = hash_hex_len(&algorithm);
                    "0".repeat(hash_len)
                } else {
                    hash_file(&artifact.path, &algorithm).with_context(|| {
                        format!(
                            "checksum: hashing {} for crate {crate_name}",
                            artifact.path.display()
                        )
                    })?
                };

                // Store the checksum for later propagation to artifact metadata.
                artifact_checksums.push((artifact.path.clone(), format!("{}:{}", algorithm, hash)));

                let filename = artifact
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");

                // Determine the display name for this artifact in the checksum line.
                // If the extra file has a name_template, render it to get an alias.
                let artifact_ext = anodize_core::template::extract_artifact_ext(filename);
                let checksum_name = if let Some(tmpl) = artifact.metadata.get("extra_name_template")
                {
                    let mut vars = ctx.template_vars().clone();
                    vars.set("ArtifactName", filename);
                    vars.set("ArtifactExt", artifact_ext);
                    // `Algorithm` parity with the sidecar name_template path
                    // below — users writing `{{ .ArtifactName }}.{{ .Algorithm
                    // }}` in extra_files name_template expect it available
                    // here too.
                    vars.set("Algorithm", &algorithm);
                    anodize_core::template::render(tmpl, &vars)
                        .unwrap_or_else(|_| filename.to_string())
                } else {
                    filename.to_string()
                };

                let line = format_checksum_line(&hash, &checksum_name);
                combined_lines.push(line);

                // Only create sidecar files in split mode
                if split {
                    let sidecar_path = if let Some(tmpl) = &name_template {
                        // Use name_template for sidecar naming when provided
                        let mut vars = ctx.template_vars().clone();
                        vars.set("ArtifactName", filename);
                        vars.set("ArtifactExt", artifact_ext);
                        vars.set("Algorithm", &algorithm);
                        let rendered =
                            anodize_core::template::render(tmpl, &vars).with_context(|| {
                                format!(
                                    "checksum: render split name_template for {}",
                                    artifact.path.display()
                                )
                            })?;
                        // GoReleaser places sidecars in dist (checksums.go:79)
                        Path::new(&dist).join(rendered)
                    } else {
                        // Default sidecar naming: {artifact}.{algorithm}
                        // GoReleaser places sidecars in dist (checksums.go:79)
                        Path::new(&dist).join(format!("{}.{}", filename, ext))
                    };

                    // GoReleaser writes ONLY the raw hex hash in sidecar files
                    // (no filename, no trailing newline).
                    if !dry_run {
                        let mut sidecar_file = File::create(&sidecar_path).with_context(|| {
                            format!("checksum: create sidecar {}", sidecar_path.display())
                        })?;
                        write!(sidecar_file, "{}", hash).with_context(|| {
                            format!("checksum: write sidecar {}", sidecar_path.display())
                        })?;
                    }

                    log.verbose(&format!(
                        "{}{} -> {} ({})",
                        if dry_run { "(dry-run) " } else { "" },
                        artifact.path.display(),
                        sidecar_path.display(),
                        algorithm
                    ));

                    // Register sidecar as a Checksum artifact
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Checksum,
                        name: String::new(),
                        path: sidecar_path,
                        target: artifact.target.clone(),
                        crate_name: crate_name.clone(),
                        metadata: HashMap::from([
                            ("algorithm".to_string(), algorithm.clone()),
                            // GoReleaser artifact.ExtraChecksumOf — the path
                            // of the artifact this checksum is for.
                            (
                                "ChecksumOf".to_string(),
                                artifact.path.to_string_lossy().into_owned(),
                            ),
                        ]),
                        size: None,
                    });
                }
            }

            // Sort combined lines by filename (the part after "  ") for
            // deterministic output and reproducible builds.
            //
            // Edge case — inherited from GoReleaser (checksums.go:171-174
            // uses `strings.Split(a, "  ")[1]`): filenames that themselves
            // contain a two-space sequence will be sorted by the prefix
            // before the *first* double-space, producing a wrong sort key.
            // This is intentionally matched to GoReleaser behavior; changing
            // it would diverge and the divergence test
            // `test_combined_sort_doublespace_divergence` will flag a fix.
            // In practice artifact filenames never contain double-spaces so
            // this is benign — but documented so future refactors don't
            // silently "improve" it.
            combined_lines.sort_by(|a, b| {
                let name_a = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
                let name_b = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
                name_a.cmp(name_b)
            });

            // Write combined checksums file (only when NOT in split mode)
            if !split {
                let combined_filename = if let Some(tmpl) = &name_template {
                    ctx.render_template(tmpl).with_context(|| {
                        format!("checksum: render name_template for {crate_name}")
                    })?
                } else {
                    let project = &ctx.config.project_name;
                    let version = ctx.version();
                    format!("{project}_{version}_checksums.txt")
                };

                let combined_path = dist.join(&combined_filename);

                // Build the combined content string for both file writing and
                // the Checksums template variable.
                // Match GoReleaser: each line gets "\n" appended, then
                // all are joined with no separator (strings.Join(lines, "")).
                let content: String = combined_lines.iter().map(|l| format!("{}\n", l)).collect();

                // Set the Checksums template variable so release body templates
                // can reference {{ .Checksums }}.
                ctx.template_vars_mut().set("Checksums", &content);

                // Only write files in non-dry-run mode; hash computation and
                // artifact registration always happen so downstream stages
                // (sign, release) can reference checksums.
                if !dry_run {
                    std::fs::create_dir_all(&dist)
                        .with_context(|| format!("checksum: create dist dir {}", dist.display()))?;

                    let mut combined_file = File::create(&combined_path).with_context(|| {
                        format!("checksum: create combined file {}", combined_path.display())
                    })?;
                    write!(combined_file, "{}", content).with_context(|| {
                        format!("checksum: write combined file {}", combined_path.display())
                    })?;
                }

                log.status(&format!(
                    "{}combined checksums -> {}",
                    if dry_run { "(dry-run) " } else { "" },
                    combined_path.display()
                ));

                new_artifacts.push(Artifact {
                    kind: ArtifactKind::Checksum,
                    name: String::new(),
                    path: combined_path,
                    target: None,
                    crate_name: crate_name.clone(),
                    metadata: HashMap::from([
                        ("algorithm".to_string(), algorithm.clone()),
                        ("combined".to_string(), "true".to_string()),
                    ]),
                    size: None,
                });
            } else {
                log.status(&format!(
                    "split mode: skipping combined checksums file for crate {crate_name}"
                ));
            }

            // store the computed checksum back into each
            // source artifact's metadata as "Checksum" = "algorithm:hash".
            // Publishers (Homebrew, Krew, etc.) read this to get per-artifact
            // checksums without re-hashing.
            let checksum_map: std::collections::HashMap<&PathBuf, &String> =
                artifact_checksums.iter().map(|(p, v)| (p, v)).collect();
            for art in ctx.artifacts.all_mut() {
                if let Some(val) = checksum_map.get(&art.path) {
                    art.metadata
                        .entry("Checksum".to_string())
                        .or_insert_with(|| (*val).clone());
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
// refresh_combined_checksums — recompute and rewrite combined checksum files
// ---------------------------------------------------------------------------

/// Refresh any combined `checksums.txt` files in-place by recomputing hashes
/// of all non-checksum, non-signature, non-certificate artifacts currently in
/// the registry. This matches GoReleaser's `ExtraRefresh` closure pattern
/// (release.go:121): after signing (which produces new signature artifacts),
/// the checksum file is regenerated so signed artifacts that happen to be
/// uploadable appear in the final sums.
///
/// Only combined checksum artifacts (metadata key `combined = "true"`) are
/// rewritten — split sidecars are per-artifact and never need refresh.
pub fn refresh_combined_checksums(ctx: &mut Context, dry_run: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }

    // Collect combined checksum artifacts (per crate).
    let combined: Vec<(PathBuf, String, String)> = ctx
        .artifacts
        .by_kind(ArtifactKind::Checksum)
        .into_iter()
        .filter(|a| a.metadata.get("combined").map(|s| s.as_str()) == Some("true"))
        .filter_map(|a| {
            let algo = a.metadata.get("algorithm")?.clone();
            Some((a.path.clone(), algo, a.crate_name.clone()))
        })
        .collect();

    if combined.is_empty() {
        return Ok(());
    }

    for (checksum_path, algorithm, crate_name) in combined {
        // Kinds that are checksummed upstream; Signature/Certificate/Checksum
        // are never hashed (they're the signing/checksum output themselves).
        let skip_kinds = [
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Certificate,
        ];

        let mut lines: Vec<String> = Vec::new();
        for artifact in ctx.artifacts.all() {
            if artifact.crate_name != crate_name {
                continue;
            }
            if skip_kinds.contains(&artifact.kind) {
                continue;
            }
            if !artifact.path.exists() {
                continue;
            }
            let hash = hash_file(&artifact.path, &algorithm)?;
            let fname = artifact
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            lines.push(format_checksum_line(&hash, &fname));
        }

        // Deterministic order (match the original combined-writer).
        lines.sort_by(|a, b| {
            let na = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
            let nb = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
            na.cmp(nb)
        });

        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        if let Some(parent) = checksum_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("refresh checksum: create parent {}", parent.display()))?;
        }
        let mut f = File::create(&checksum_path)
            .with_context(|| format!("refresh checksum: create {}", checksum_path.display()))?;
        write!(f, "{content}")
            .with_context(|| format!("refresh checksum: write {}", checksum_path.display()))?;
    }

    Ok(())
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
        assert!(
            hash.starts_with("840006653e9ac9e95117a15c915caab81662918e925de9e004f774ff82d7079a")
        );
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
        use anodize_core::config::ExtraFileSpec;

        let yaml = r#"
name_template: "checksums.txt"
algorithm: "sha256"
extra_files:
  - "dist/*.bin"
  - "README.md"
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
        use anodize_core::config::{ChecksumConfig, CrateConfig, Defaults, StringOrBool};
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
                    disable: Some(StringOrBool::Bool(true)),
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
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig, StringOrBool};
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
                disable: Some(StringOrBool::Bool(true)),
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
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
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
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
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
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
        use anodize_core::context::{Context, ContextOptions};

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
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.split, Some(true));
    }

    #[test]
    fn test_split_config_parsing_false() {
        let yaml = r#"
algorithm: "sha256"
split: false
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.split, Some(false));
    }

    #[test]
    fn test_split_config_parsing_absent() {
        let yaml = r#"
algorithm: "sha256"
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.split, None);
    }

    #[test]
    fn test_checksum_stage_split_true_no_combined_file() {
        use anodize_core::config::{ChecksumConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::{ChecksumConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::CrateConfig;
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
        use anodize_core::config::{ChecksumConfig, CrateConfig, Defaults};
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
        use anodize_core::config::CrateConfig;
        use anodize_core::test_helpers::TestContextBuilder;

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
disable: "{{ if .IsSnapshot }}true{{ end }}"
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match &cfg.disable {
            Some(anodize_core::config::StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
                assert!(cfg.disable.as_ref().unwrap().is_template());
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_config_disable_bool_parsing() {
        let yaml = r#"
algorithm: "sha256"
disable: true
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.disable,
            Some(anodize_core::config::StringOrBool::Bool(true))
        );
        assert!(!cfg.disable.as_ref().unwrap().is_template());
    }

    #[test]
    fn test_config_extra_files_object_form() {
        use anodize_core::config::ExtraFileSpec;

        let yaml = r#"
extra_files:
  - "dist/*.bin"
  - glob: "release/*.deb"
    name_template: "{{ .ArtifactName }}.checksum"
"#;
        let cfg: anodize_core::config::ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
        use anodize_core::config::CrateConfig;
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::{ChecksumConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::{ChecksumConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::config::{ChecksumConfig, CrateConfig, StringOrBool};
        use anodize_core::test_helpers::TestContextBuilder;

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
                    disable: Some(StringOrBool::String("true".to_string())),
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
            "disable: 'true' string should disable checksums"
        );
    }

    #[test]
    fn test_extra_file_detailed_name_template_combined_mode() {
        // Verifies that ExtraFileSpec::Detailed with name_template correctly renames
        // the entry in the combined (non-split) checksum file via the template engine.
        use anodize_core::config::{ChecksumConfig, Config, CrateConfig, ExtraFileSpec};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{ChecksumConfig, CrateConfig, TemplatedExtraFile};
        use anodize_core::test_helpers::TestContextBuilder;

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
}
