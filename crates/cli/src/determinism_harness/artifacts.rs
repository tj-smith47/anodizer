//! Per-run artifact discovery, hashing, and drift-bin dump/prune.
//!
//! - [`discover_artifacts`] walks `<worktree>/dist` and surfaces the
//!   raw cargo binaries under `<worktree>/.det-tmp/target/`.
//! - [`hash_artifacts`] SHA256s every artifact and returns
//!   `{name -> ArtifactInfo}` (hash + size + path + stage
//!   attribution + head/tail samples).
//! - [`copy_artifacts_to_dump`] / [`prune_dump_to_drifted`] dump the
//!   per-run binaries to `<report_parent>/drift-bins/run-<N>/` and
//!   then keep only the drifted ones so the artifact upload stays
//!   compact while preserving the diagnostic escape hatch.
//! - [`preserve_dist_tree`] / [`write_preserved_dist_context`] support
//!   the `--preserve-dist=<path>` flag: copy `<worktree>/dist/**` to
//!   an operator-supplied destination during run-0 and emit a
//!   `context.json` manifest the publish-only path consumes. Spec:
//!   `.claude/specs/2026-05-19-determinism-produces-shippable.md`.

use anodizer_core::DeterminismReport;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-run artifact info captured by [`hash_artifacts`]. Internal to
/// the parent module; `pub(super)` so `Harness::build_report` can read
/// `hash` / `size_bytes` / `relative_path` / `stage` and
/// [`super::drift::summarize_drift`] can read the head/tail samples.
#[derive(Debug, Clone)]
pub(super) struct ArtifactInfo {
    pub(super) hash: String,
    pub(super) size_bytes: u64,
    /// Path relative to the worktree root (with leading `dist/` etc).
    /// Used as the canonical `ArtifactRow.path` value.
    pub(super) relative_path: String,
    /// Best-effort stage attribution from the path prefix.
    pub(super) stage: String,
    /// First [`HEAD_SAMPLE_BYTES`] bytes of the artifact, retained so
    /// the harness can populate `DriftRow.differing_bytes_summary`
    /// after the worktree is dropped. Why a head sample (not the full
    /// content): the largest artifact in the pipeline is the raw
    /// `.exe` at ~50 MB; multiplied by N runs and ~50 artifacts/run
    /// the retained bytes would blow past the report file's useful
    /// size. The head is what matters for PE / archive / Mach-O drift
    /// (their metadata is front-loaded), and the sample is read
    /// once during the existing `std::fs::read` so there's no extra
    /// I/O.
    pub(super) head_sample: Vec<u8>,
    /// Last [`TAIL_SAMPLE_BYTES`] bytes of the artifact. Complements
    /// `head_sample`: trailing structures that drift past 1 KiB —
    /// gzip footer (`mtime`, ISIZE), zstd skippable frames, ZIP
    /// central directory, PE Debug Directory contents, detached
    /// signature `.sig` trailers — get a localized offset instead of
    /// `"no diff in first 1 KiB"`. Empty when the artifact is smaller
    /// than the head window (the head already covers the whole file).
    pub(super) tail_sample: Vec<u8>,
}

/// How many leading bytes of each artifact to retain for drift
/// diagnostics. 16 KiB covers:
///   - PE: DOS stub + PE signature + COFF header + Optional header +
///     several pages of the .text section. Catches `TimeDateStamp`,
///     `MajorLinkerVersion`, debug directory RVA, and the Rich header.
///   - tar.gz: gzip header + first tar entry header + early file
///     bodies. Catches gzip `mtime` and tar `mtime` drift.
///   - zip: local file header + filename + first file's data start.
///   - CycloneDX SBOM JSON: top-level keys including
///     `serialNumber` (per-run UUID — a known drift source).
pub(super) const HEAD_SAMPLE_BYTES: usize = 16 * 1024;

/// How many trailing bytes of each artifact to retain alongside the
/// head sample. Catches trailing-section drift that the head misses:
///   - gzip footer: 4-byte `mtime` + 4-byte ISIZE.
///   - zstd: skippable frames + content checksum (last 4 B).
///   - ZIP: central directory record + end-of-central-directory
///     record (`EOCD`) including the per-archive comment.
///   - PE: Debug Directory contents (GUID + age + PDB path), import
///     address table, resource section drift.
///   - Detached signatures (`.sig`): cosign/gpg signature blob lives
///     entirely past the head window.
pub(super) const TAIL_SAMPLE_BYTES: usize = 16 * 1024;

/// Walk `<worktree>/dist` and collect every regular file. Sorted by path
/// for deterministic iteration order in tests.
///
/// Also surfaces the **raw cargo build outputs** at
/// `<worktree>/.det-tmp/target/<triple>/release/<bin>` (or
/// `<worktree>/.det-tmp/target/release/<bin>` when the build wasn't
/// `--target`-pinned). These are the SOURCE of any RUSTFLAGS / mtime /
/// build-script drift that later propagates into every wrapped archive
/// (`.tar.gz`, `.tar.xz`, `.zip`, ...). Hashing them directly lets the
/// report point a finger at the raw binary instead of the operator
/// having to peel six layers of containers to find that the underlying
/// `target/release/anodize` was nondeterministic. Path-remapping
/// (`--remap-path-prefix`) is already applied via the env block, so on
/// a healthy run these hashes will match; if they ever drift, we want
/// the diagnostic chain to start here.
///
/// The function only walks the immediate `release/` directory (not
/// `deps/`, `build/`, `.fingerprint/`, etc.) and filters to files
/// without an extension or with `.exe` — anodize ships single-binary
/// crates, so this surfaces the actual `anodize` / `anodize.exe`
/// without dragging in cargo's incremental-build scratch.
pub(super) fn discover_artifacts(worktree_path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let dist = worktree_path.join("dist");
    if dist.exists() {
        visit_dir(&dist, &mut out)?;
    }

    let target_root = worktree_path.join(".det-tmp").join("target");
    if target_root.exists() {
        collect_raw_binaries(&target_root, &mut out)?;
    }

    out.sort();
    Ok(out)
}

fn visit_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            visit_dir(&entry.path(), out)?;
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Collect raw cargo release binaries from `<cargo_target>/[<triple>/]release/`.
///
/// Two layouts to support:
///
/// - `<cargo_target>/release/<bin>` — host build, no `--target` flag.
/// - `<cargo_target>/<triple>/release/<bin>` — cross-target build.
///
/// We only emit the top-level files inside each `release/` directory.
/// `release/deps`, `release/build`, `release/.fingerprint`, etc. are
/// cargo's internal scratch and not what we want to fingerprint for
/// drift detection.
///
/// File filter: regular files whose extension is empty (`anodize`) or
/// `.exe` (`anodize.exe`). Excludes `.d` (depfiles), `.pdb` (debug
/// symbols), `.rlib`, etc. — those are tooling byproducts, not the
/// shippable binary that lands in archives.
fn collect_raw_binaries(target_root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(target_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target_root.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name_s == "release" {
            push_release_dir_files(&entry.path(), out)?;
        } else if name_s == "debug"
            || name_s == ".rustc_info.json"
            || name_s == "CACHEDIR.TAG"
            || name_s.starts_with('.')
        {
            continue;
        } else {
            let release_dir = entry.path().join("release");
            if release_dir.is_dir() {
                push_release_dir_files(&release_dir, out)?;
            }
        }
    }
    Ok(())
}

fn push_release_dir_files(release_dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(release_dir)
        .with_context(|| format!("reading {}", release_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        match path.extension().and_then(|s| s.to_str()) {
            None => out.push(path),
            Some("exe") => out.push(path),
            _ => continue,
        }
    }
    Ok(())
}

/// SHA256 every artifact and return `{name -> info}`.
///
/// Map keys are usually the artifact basename (matching the spec's
/// allow-list pattern semantics — glob/exact matches operate on the
/// file name). Raw cargo binaries under `<worktree>/.det-tmp/target`
/// get a `target/<triple>/<bin>` (or `target/release/<bin>` for host
/// builds) prefix so the report unambiguously distinguishes
/// `dist/anodize` (the shipped binary inside an archive) from
/// `target/<triple>/anodize` (the raw cargo output that flows INTO
/// the archive). Without the prefix, a reader of the report can't
/// tell which file's hash they're looking at when both kinds exist.
pub(super) fn hash_artifacts(
    worktree_path: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<String, ArtifactInfo>> {
    use sha2::{Digest, Sha256};
    let mut out = BTreeMap::new();
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let bytes =
            std::fs::read(p).with_context(|| format!("reading artifact {}", p.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = format!("sha256:{:x}", hasher.finalize());
        let relative = p
            .strip_prefix(worktree_path)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned();
        let name = if let Ok(under_target) = p.strip_prefix(&target_root) {
            // Raw cargo binary: prefix with `target/` and the
            // <triple>/release/ (or release/) segments so the report
            // surfaces it distinctly from any `dist/` artifact of the
            // same basename. Forward slashes regardless of platform
            // (matches `Artifact::to_artifacts_json` normalization).
            let suffix = under_target.to_string_lossy().replace('\\', "/");
            format!("target/{}", suffix)
        } else {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let stage = infer_stage_from_path(&relative);
        let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
        let head_sample = bytes[..head_len].to_vec();
        // Tail sample is non-overlapping with the head: when the file
        // is smaller than HEAD + TAIL, the head already covers the
        // whole content and the tail is left empty so the drift
        // summary doesn't double-count bytes.
        let tail_sample = if bytes.len() > HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES {
            bytes[bytes.len() - TAIL_SAMPLE_BYTES..].to_vec()
        } else {
            Vec::new()
        };
        out.insert(
            name,
            ArtifactInfo {
                hash: digest,
                size_bytes: bytes.len() as u64,
                relative_path: relative,
                stage,
                head_sample,
                tail_sample,
            },
        );
    }
    Ok(out)
}

/// Copy each artifact in `paths` to `dump_root/<artifact-name>`,
/// preserving the relative directory structure under `worktree_path`.
///
/// Best-effort: copy failures are logged but not surfaced, so the
/// harness's primary determinism check is never broken by a side
/// channel diagnostic.
pub(super) fn copy_artifacts_to_dump(
    worktree_path: &Path,
    paths: &[PathBuf],
    dump_root: &Path,
) -> Result<()> {
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let dest_rel = if let Ok(under_target) = p.strip_prefix(&target_root) {
            PathBuf::from("target").join(under_target)
        } else if let Ok(under_worktree) = p.strip_prefix(worktree_path) {
            under_worktree.to_path_buf()
        } else {
            PathBuf::from(p.file_name().unwrap_or_default())
        };
        let dest = dump_root.join(dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dump parent {}", parent.display()))?;
        }
        if let Err(e) = std::fs::copy(p, &dest) {
            eprintln!(
                "warn: drift-bin dump failed for {} -> {}: {}",
                p.display(),
                dest.display(),
                e
            );
        }
    }
    Ok(())
}

/// Prune `<dump_root>/run-<N>/<artifact>` entries whose artifact name
/// does NOT appear in `report.drift`. Keeps the artifact upload
/// compact (drifted binaries only) without sacrificing the per-run
/// dump that the harness captured pre-comparison.
pub(super) fn prune_dump_to_drifted(dump_root: &Path, report: &DeterminismReport) {
    if !dump_root.exists() {
        return;
    }
    let drift_names: std::collections::HashSet<&str> =
        report.drift.iter().map(|d| d.artifact.as_str()).collect();
    let Ok(run_dirs) = std::fs::read_dir(dump_root) else {
        return;
    };
    for run_entry in run_dirs.flatten() {
        let run_path = run_entry.path();
        if !run_path.is_dir() {
            continue;
        }
        prune_dump_subtree(&run_path, &run_path, &drift_names);
    }
}

fn prune_dump_subtree(root: &Path, dir: &Path, drift_names: &std::collections::HashSet<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_dump_subtree(root, &path, drift_names);
            if std::fs::read_dir(&path)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&path);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !drift_names.contains(rel.as_str()) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Best-effort stage attribution from the artifact path. The harness
/// does not have access to the pipeline's per-stage Artifact records (it
/// shells to a child process), so it infers from filename extension and
/// path conventions. Falls back to `"unknown"` when nothing matches.
pub(super) fn infer_stage_from_path(rel: &str) -> String {
    let lower = rel.replace('\\', "/").to_lowercase();
    // Raw cargo build output under `<worktree>/.det-tmp/target/...` —
    // attribute to `build` so the report makes the source-of-drift
    // chain explicit (build → archive → checksum → sign).
    if lower.contains("/.det-tmp/target/") || lower.starts_with(".det-tmp/target/") {
        return "build".into();
    }
    if lower.ends_with(".sig") || lower.ends_with(".pem") || lower.ends_with(".cert") {
        "sign".into()
    } else if lower.contains("checksums")
        || lower.ends_with("sha256sum")
        || lower.ends_with("sha256sums")
        || lower.ends_with(".sha256")
    {
        "checksum".into()
    } else if lower.ends_with(".sbom.json")
        || lower.ends_with(".cdx.json")
        || lower.ends_with(".spdx.json")
    {
        "sbom".into()
    } else if lower.ends_with(".tar.gz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".tar.zst")
        || lower.ends_with(".zip")
        || lower.ends_with(".tar")
    {
        "archive".into()
    } else if lower.ends_with(".crate") {
        "cargo-package".into()
    } else {
        "unknown".into()
    }
}

/// One artifact entry in [`PreservedDistContext::artifacts`]. The shape
/// mirrors the load-bearing subset of
/// [`crate::commands::release::split::SplitArtifact`] — `name`, `path`,
/// `target` — and adds two harness-specific fields (`sha256`, `size`)
/// the publish-only path uses to verify that the preserved bytes match
/// the determinism check's recorded hashes before re-signing fires.
///
/// We deliberately do NOT reuse `SplitArtifact` directly: the harness
/// runs as a subprocess of `anodize release` and never instantiates the
/// in-process `Context::artifacts` registry, so it has no `ArtifactKind`
/// / `crate_name` / `metadata` to populate. Replicating just the fields
/// we can populate keeps `context.json` honest about what the harness
/// observed, and a Phase-2 consumer can deserialize either shape (the
/// fields we DO emit have the same names + types as the corresponding
/// `SplitArtifact` fields).
///
/// Spec: `.claude/specs/2026-05-19-determinism-produces-shippable.md`
/// section A.3.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreservedArtifact {
    /// Artifact filename (basename). Matches `SplitArtifact::name`.
    pub name: String,
    /// Path relative to the preserved-dist root (e.g.
    /// `anodizer_0.3.0_linux_amd64.tar.gz` or
    /// `checksums/SHA256SUMS`). Matches `SplitArtifact::path` modulo the
    /// relative-vs-absolute axis: split stores absolute worktree paths,
    /// the preserved manifest stores paths under the preserved-dist
    /// root so a downstream consumer can join against `<dest>/`.
    pub path: String,
    /// SHA256 of the artifact bytes, prefixed `sha256:` (matches the
    /// `DeterminismReport.artifacts[].hash` format so a publish-only
    /// consumer can verify preserved bytes against the determinism
    /// report's recorded hashes without re-deriving the digest).
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

/// Manifest the `--preserve-dist=<path>` flag emits to
/// `<dest>/context.json` once the harness greens.
///
/// Schema mirrors the load-bearing subset of
/// [`crate::commands::release::split::SplitContext`]: `artifacts`,
/// `targets`, `version`, `commit`. The publish-only pipeline
/// (Phase 2 of the spec) reads this file to rehydrate
/// `ctx.artifacts` + the per-target matrix before running the sign +
/// publish stages.
///
/// Field reuse rationale matches [`PreservedArtifact`] — same field
/// names + types where applicable, so a future consumer can
/// deserialize either format via `#[serde(default)]` migrations.
///
/// Spec: `.claude/specs/2026-05-19-determinism-produces-shippable.md`
/// section A.3.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreservedDistContext {
    /// Artifact set the harness preserved. Sorted by `name` so the
    /// JSON output is reproducible across runs.
    pub artifacts: Vec<PreservedArtifact>,
    /// Target triples the harness exercised. Pulled from
    /// `dist/artifacts.json:target` (union over all artifacts that
    /// declared one) so it's the set actually built, not the set
    /// configured.
    pub targets: Vec<String>,
    /// Release version string. Read from `<dist>/metadata.json:version`
    /// when present (the snapshot/release pipeline writes that file
    /// pre-release). Empty when the harness ran against a fixture
    /// without a metadata.json (no `version` field to recover).
    pub version: String,
    /// Full commit SHA the harness rebuilt — populated by the harness
    /// from its `Harness::commit` field so the manifest is
    /// self-contained (no need to re-resolve from git).
    pub commit: String,
}

/// Copy `<worktree>/dist/**` to `dest`, preserving directory structure.
/// Creates `dest` if missing and overwrites any existing files (the
/// harness owns this path between runs and across iterations).
///
/// Best-effort safety: clear `dest` before populating so a leftover
/// from a prior aborted run can't shadow run-0's actual output. If
/// `dest` doesn't exist yet, `remove_dir_all` is a no-op (it returns
/// `NotFound`, swallowed here).
///
/// Called from `Harness::run` between run-0's hashing and the
/// next iteration's `Worktree` destruction. Spec: section A.2.
pub(super) fn preserve_dist_tree(worktree_path: &Path, dest: &Path) -> Result<()> {
    let src = worktree_path.join("dist");
    // The src/dest is a directory tree; clear dest first so we don't
    // mingle bytes from a prior aborted preservation attempt.
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .with_context(|| format!("clearing stale preserved-dist at {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating preserved-dist root at {}", dest.display()))?;
    if !src.exists() {
        // No dist/ inside the worktree — harness ran a build that
        // produced nothing under dist/. Caller (build_report) will
        // surface this via an empty `artifacts` list; we keep the
        // dest dir so context.json can still land.
        return Ok(());
    }
    copy_dir_recursive(&src, dest)
        .with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
    Ok(())
}

/// Recursive directory copy with predictable semantics — files via
/// `std::fs::copy`, directories created on demand. Symlinks are
/// dereferenced (harness output should not contain symlinks; this is
/// defensive).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("creating destination dir {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("reading source dir {}", src.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            // is_file() OR symlink dereferenced via fs::copy. fs::copy
            // follows symlinks and copies content, which is what we want
            // for a hermetic preserved artifact set.
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copying {} → {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

/// Write `<dest>/context.json` describing the preserved artifact set.
///
/// Pulls per-artifact `sha256` + `size_bytes` from the determinism
/// report's `artifacts` array (the harness already hashed every file;
/// re-hashing here would be wasteful). Pulls `targets` from
/// `<dest>/artifacts.json` when present (written by the release
/// pipeline pre-release) and `version` from `<dest>/metadata.json`
/// (same source).
///
/// Spec: section A.3.
pub(super) fn write_preserved_dist_context(dest: &Path, report: &DeterminismReport) -> Result<()> {
    // dist/artifacts.json: rich per-artifact metadata (goos / goarch /
    // target / kind). Optional — fixture builds without a configured
    // crate emit dist/ contents but not artifacts.json. When missing,
    // the manifest still ships with sha256 / size from the report.
    let artifacts_json: Option<serde_json::Value> = {
        let path = dest.join("artifacts.json");
        if path.exists() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {} as JSON", path.display()))?,
            )
        } else {
            None
        }
    };
    let targets: Vec<String> = artifacts_json
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for entry in arr {
                if let Some(t) = entry.get("target").and_then(|t| t.as_str())
                    && !t.is_empty()
                {
                    seen.insert(t.to_string());
                }
            }
            seen.into_iter().collect()
        })
        .unwrap_or_default();

    // dist/metadata.json: { project_name, tag, version, commit }
    let version: String = {
        let path = dest.join("metadata.json");
        if path.exists() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {} as JSON", path.display()))?;
            v.get("version")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        }
    };

    // Build the preserved-artifact list from a walk of `dest/**`, so
    // every preserved file is described — including metadata.json /
    // artifacts.json / checksums (whatever the harness produced).
    // sha256 / size come from the report's `artifacts` array (keyed by
    // basename, matching the harness's hash-map keys) when available;
    // files that don't appear in the report (e.g. context.json itself
    // doesn't exist yet, but metadata.json is freshly written and may
    // not have been hashed if it landed after the discover walk) are
    // hashed lazily here so the manifest is complete.
    let report_by_name: std::collections::HashMap<&str, &anodizer_core::ArtifactRow> = report
        .artifacts
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    let mut entries: Vec<PreservedArtifact> = Vec::new();
    collect_preserved_entries(dest, dest, &report_by_name, &mut entries)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let ctx = PreservedDistContext {
        artifacts: entries,
        targets,
        version,
        commit: report.commit.clone(),
    };
    let json =
        serde_json::to_string_pretty(&ctx).context("serializing PreservedDistContext to JSON")?;
    std::fs::write(dest.join("context.json"), json)
        .with_context(|| format!("writing context.json under {}", dest.display()))?;
    Ok(())
}

fn collect_preserved_entries(
    root: &Path,
    dir: &Path,
    report_by_name: &std::collections::HashMap<&str, &anodizer_core::ArtifactRow>,
    out: &mut Vec<PreservedArtifact>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading preserved-dist dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_preserved_entries(root, &path, report_by_name, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip context.json itself — we're writing it; it shouldn't
        // describe itself (the chicken-and-egg would force a re-hash
        // anyway).
        if name == "context.json" {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let (sha256, size) = if let Some(row) = report_by_name.get(name.as_str())
            && let Some(hash) = row.hash.as_ref()
        {
            (hash.clone(), row.size_bytes)
        } else {
            // Fall back to a fresh hash — file is present in the
            // preserved tree but wasn't surfaced by the harness's
            // discover walk (or had drifted/missing hash). Better to
            // ship a complete manifest than skip the entry.
            hash_file(&path)?
        };
        out.push(PreservedArtifact {
            name,
            path: rel,
            sha256,
            size,
        });
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .with_context(|| format!("hashing preserved artifact {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok((
        format!("sha256:{:x}", hasher.finalize()),
        bytes.len() as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_inference_matches_known_extensions() {
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.zip"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.crate"), "cargo-package");
        assert_eq!(infer_stage_from_path("dist/foo.sbom.json"), "sbom");
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz.sig"), "sign");
        assert_eq!(infer_stage_from_path("dist/checksums.txt"), "checksum");
        assert_eq!(infer_stage_from_path("dist/SHA256SUMS"), "checksum");
        assert_eq!(infer_stage_from_path("dist/mystery.bin"), "unknown");
        // Windows-native separators must still classify correctly.
        assert_eq!(
            infer_stage_from_path(".det-tmp\\target\\x86_64-pc-windows-msvc\\release\\anodize.exe"),
            "build"
        );
        assert_eq!(infer_stage_from_path("dist\\foo.tar.gz"), "archive");
    }

    /// `discover_artifacts` MUST surface raw cargo binaries from
    /// `<worktree>/.det-tmp/target/<triple>/release/<bin>` AND
    /// `<worktree>/.det-tmp/target/release/<bin>`, alongside `dist/`
    /// artifacts, with the raw binaries getting a `target/...` map key
    /// prefix so the report distinguishes them from any same-basename
    /// `dist/` files. Closes the diagnostic gap where binary-level
    /// RUSTFLAGS / mtime drift was only observable through six layers
    /// of wrapper archives.
    #[test]
    fn discover_artifacts_includes_raw_cargo_binaries() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();

        // dist artifact (existing surface)
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("anodize_0.3.0_linux_amd64.tar.gz"), b"archive").unwrap();

        // Cross-target build outputs
        let triple_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-unknown-linux-gnu")
            .join("release");
        std::fs::create_dir_all(&triple_release).unwrap();
        std::fs::write(triple_release.join("anodize"), b"raw-bin-linux").unwrap();
        // depfile must NOT be surfaced (cargo scratch).
        std::fs::write(triple_release.join("anodize.d"), b"depfile").unwrap();
        // `deps/` subdirectory must NOT be recursed (cargo scratch).
        std::fs::create_dir_all(triple_release.join("deps")).unwrap();
        std::fs::write(triple_release.join("deps").join("libfoo.rlib"), b"rlib").unwrap();

        // Windows-style triple with .exe
        let win_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-pc-windows-msvc")
            .join("release");
        std::fs::create_dir_all(&win_release).unwrap();
        std::fs::write(win_release.join("anodize.exe"), b"raw-bin-windows").unwrap();
        // .pdb debug symbols must NOT be surfaced.
        std::fs::write(win_release.join("anodize.pdb"), b"pdb").unwrap();

        // Host build (no triple): target/release/anodize.
        let host_release = wt.join(".det-tmp").join("target").join("release");
        std::fs::create_dir_all(&host_release).unwrap();
        std::fs::write(host_release.join("anodize"), b"raw-bin-host").unwrap();

        let artifacts = discover_artifacts(wt).expect("discover");
        let names: Vec<String> = artifacts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            names
                .iter()
                .any(|n| n == "anodize_0.3.0_linux_amd64.tar.gz"),
            "dist artifact missing: {names:?}"
        );
        // Three raw binaries: linux triple, windows triple, host release.
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "anodize").count(),
            2,
            "expected 2 `anodize` raw binaries (linux + host), got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "anodize.exe"),
            "windows raw binary missing: {names:?}"
        );

        // Scratch files must NOT be surfaced.
        for forbidden in ["anodize.d", "anodize.pdb", "libfoo.rlib"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "cargo scratch `{forbidden}` leaked into discovery: {names:?}"
            );
        }

        // hash_artifacts must label the raw binaries with a `target/...`
        // map key so the report distinguishes them from `dist/`.
        let map = hash_artifacts(wt, &artifacts).expect("hash");
        let target_keys: Vec<&String> = map.keys().filter(|k| k.starts_with("target/")).collect();
        assert_eq!(
            target_keys.len(),
            3,
            "expected 3 `target/...`-prefixed map keys, got: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        // Forward slashes regardless of host platform.
        for k in &target_keys {
            assert!(
                !k.contains('\\'),
                "raw-binary map key contains backslash: {k}"
            );
        }
        // Spot-check one key shape.
        assert!(
            target_keys
                .iter()
                .any(|k| { k.as_str() == "target/x86_64-unknown-linux-gnu/release/anodize" }),
            "expected `target/x86_64-unknown-linux-gnu/release/anodize` key, got: {target_keys:?}"
        );
        // Raw binaries get `build` stage attribution so the diagnostic
        // chain reads build → archive → checksum → sign.
        for k in &target_keys {
            assert_eq!(
                map.get(k.as_str()).map(|i| i.stage.as_str()),
                Some("build"),
                "raw binary `{k}` must be attributed to `build` stage"
            );
        }
    }

    /// `discover_artifacts` must tolerate a missing `.det-tmp/target`
    /// (e.g. the harness has only just spawned and the child hasn't
    /// produced anything yet) — it shouldn't error out.
    #[test]
    fn discover_artifacts_tolerates_missing_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        // Just dist/, no .det-tmp/.
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("foo.tar.gz"), b"x").unwrap();
        let out = discover_artifacts(wt).expect("must not error on missing target dir");
        assert_eq!(out.len(), 1);
    }
}
