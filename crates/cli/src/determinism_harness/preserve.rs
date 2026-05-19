//! Preserved-dist support for `anodize check determinism --preserve-dist=<path>`.
//!
//! When the harness greens, this module:
//!
//! 1. Copies `<worktree>/dist/**` from run-0 to the operator-supplied
//!    destination ([`preserve_dist_tree`]).
//! 2. After all runs finish without drift, writes
//!    `<dest>/context.json` describing the preserved artifact set
//!    ([`write_preserved_dist_context`]).
//!
//! The Phase-2 publish-only pipeline (spec
//! `.claude/specs/2026-05-19-determinism-produces-shippable.md`)
//! consumes the resulting tree directly — eliminating the redundant
//! `build:` job that currently recompiles every target ~3× per release.
//!
//! ## Why a separate module
//!
//! `artifacts.rs` owns per-run *discovery / hashing / dump-prune* — work
//! that runs inside the harness loop. Preserve-dist is end-of-loop work
//! with a different lifecycle (one-shot, runs only on the
//! green-with-flag-set path). Keeping the two concerns split keeps
//! `artifacts.rs` focused and makes the preserve-dist surface easier to
//! reason about as an integration boundary with Phase 2.

use anodizer_core::DeterminismReport;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// One artifact entry in [`PreservedDistContext::artifacts`].
///
/// Schema is a hybrid of the load-bearing fields from
/// [`crate::commands::release::split::SplitArtifact`] (`name`, `path`)
/// and two harness-specific fields (`sha256`, `size`) the publish-only
/// path uses to verify preserved bytes against the determinism check's
/// recorded hashes before re-signing fires.
///
/// **Cross-format deserialization**: `SplitArtifact` carries
/// `#[serde(default)] sha256: Option<String>` and `#[serde(default)]
/// size: Option<u64>` so a Phase-2 reader that already speaks
/// `SplitContext` can deserialize a `PreservedDistContext` cleanly
/// (extra fields ignored, missing fields default to `None`). The
/// reverse direction works too: deserializing a `SplitArtifact`-shaped
/// entry as a `PreservedArtifact` requires `sha256` / `size` to be
/// present, which they are when written by this module.
///
/// We deliberately do NOT reuse `SplitArtifact` directly: the harness
/// runs as a subprocess of `anodize release` and never instantiates the
/// in-process `Context::artifacts` registry, so it has no `ArtifactKind`
/// / `crate_name` / `metadata` to populate. Replicating just the fields
/// we can populate keeps `context.json` honest about what the harness
/// observed.
///
/// Spec: `.claude/specs/2026-05-19-determinism-produces-shippable.md`
/// section A.3.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreservedArtifact {
    /// Artifact filename (basename). Field name matches
    /// `SplitArtifact::name`.
    pub name: String,
    /// Path relative to the preserved-dist root (e.g.
    /// `anodizer_0.3.0_linux_amd64.tar.gz` or
    /// `checksums/SHA256SUMS`). Field name matches
    /// `SplitArtifact::path` modulo the relative-vs-absolute axis:
    /// split stores absolute worktree paths, the preserved manifest
    /// stores paths under the preserved-dist root so a downstream
    /// consumer can join against `<dest>/`.
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
/// Spec: `.claude/specs/2026-05-19-determinism-produces-shippable.md`
/// section A.3.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreservedDistContext {
    /// Artifact set the harness preserved. Sorted by `name` so the
    /// JSON output is reproducible across runs.
    pub artifacts: Vec<PreservedArtifact>,
    /// Target triples the harness exercised. Pulled from
    /// `<dest>/artifacts.json:target` (union over all artifacts that
    /// declared one). When that file is missing or empty of targets
    /// (e.g. fixture builds whose stages haven't tagged artifacts
    /// with a `target`), falls back to the `--targets=<csv>` value
    /// passed to the harness so `context.json` always ships with a
    /// non-empty list for the production case.
    pub targets: Vec<String>,
    /// Release version string. Read from `<dest>/metadata.json:version`
    /// (the snapshot/release pipeline writes that file via
    /// `run_post_pipeline`). Falls back to a caller-supplied default
    /// (`Harness::version_hint`) when the JSON is missing /
    /// malformed.
    pub version: String,
    /// Full commit SHA the harness rebuilt — populated by the harness
    /// from its `Harness::commit` field so the manifest is
    /// self-contained (no need to re-resolve from git).
    pub commit: String,
}

/// Copy `<worktree>/dist/**` to `dest`, preserving directory structure.
///
/// Best-effort safety: clear `dest` before populating so a leftover
/// from a prior aborted run can't shadow run-0's actual output. If
/// `dest` doesn't exist yet, the clear is a no-op.
///
/// Called from `Harness::run` between run-0's hashing and the
/// next iteration's `Worktree` destruction. Spec: section A.2.
pub(super) fn preserve_dist_tree(worktree_path: &Path, dest: &Path) -> Result<()> {
    let src = worktree_path.join("dist");
    // Clear dest first — defends against a prior aborted preservation
    // attempt that left partial bytes behind. Tolerate NotFound
    // (first-run, dest doesn't exist yet) but surface every other
    // error so a permissions/IO failure isn't silently masked into a
    // "preserved tree mingles bytes from two runs" footgun.
    match std::fs::remove_dir_all(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("clearing stale preserved-dist at {}", dest.display()));
        }
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating preserved-dist root at {}", dest.display()))?;
    // src may be absent: the harness ran a build that produced
    // nothing under dist/ (e.g. only `target/...` raw binaries). Keep
    // the dest dir so context.json can still land — caller writes it
    // post-loop regardless.
    match std::fs::read_dir(&src) {
        Ok(_) => copy_dir_recursive(&src, dest)
            .with_context(|| format!("copying {} -> {}", src.display(), dest.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading source dir {}", src.display()));
        }
    }
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
                format!("copying {} -> {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

/// Inputs to [`write_preserved_dist_context`]. Bundles the values the
/// harness owns (target list, version hint) so the function signature
/// doesn't grow per added field. The struct is internal to this module
/// plus the harness loop — public visibility kept to `pub(super)` since
/// the harness builds it inline.
pub(super) struct ContextInputs<'a> {
    pub report: &'a DeterminismReport,
    /// Harness's `--targets=<csv>` value — used as a fallback when
    /// `<dest>/artifacts.json` exists but no artifact carries a
    /// `target` field. Pass `None` when the harness ran with no
    /// filter (every configured target).
    pub harness_targets: Option<&'a [String]>,
    /// Fallback version string when `<dest>/metadata.json` is missing
    /// or malformed. Harness pulls this from its own resolved
    /// template vars; passing the empty string is acceptable for
    /// fixture/local runs that don't care about the manifest's
    /// `version` field.
    pub version_hint: &'a str,
}

/// Write `<dest>/context.json` describing the preserved artifact set.
///
/// Pulls per-artifact `sha256` + `size_bytes` from the determinism
/// report's `artifacts` array (the harness already hashed every file;
/// re-hashing here would be wasteful). Pulls `targets` from
/// `<dest>/artifacts.json` when present and `version` from
/// `<dest>/metadata.json` (the release pipeline writes both files via
/// `run_post_pipeline` even when the `release` stage is skipped, so
/// they ARE available after a successful harness run). Both reads
/// tolerate missing files and malformed JSON (warn + fall back to
/// harness-supplied defaults) so a corrupted sibling can't kill the
/// manifest write.
///
/// Write is atomic via stage-to-`.tmp` + rename, matching
/// `commands/release/split.rs::run_split`.
///
/// Spec: section A.3.
pub(super) fn write_preserved_dist_context(dest: &Path, inputs: ContextInputs<'_>) -> Result<()> {
    let report = inputs.report;

    // ── dist/artifacts.json: rich per-artifact metadata ──────────────
    // Optional + tolerant of corruption — fall back to defaults so a
    // malformed sibling JSON can't kill the manifest write.
    let artifacts_json: Option<serde_json::Value> =
        read_optional_json(&dest.join("artifacts.json"));
    let mut targets: Vec<String> = artifacts_json
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            let mut seen: BTreeSet<String> = BTreeSet::new();
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
    // Fall back to the harness's `--targets=<csv>` list when the
    // production walk produced nothing. Catches the case where the
    // child pipeline produced artifacts.json but no stage tagged
    // artifacts with a target (e.g. archive-only fixture runs).
    if targets.is_empty()
        && let Some(harness_targets) = inputs.harness_targets
    {
        let mut sorted: BTreeSet<String> = BTreeSet::new();
        for t in harness_targets {
            if !t.is_empty() {
                sorted.insert(t.clone());
            }
        }
        targets = sorted.into_iter().collect();
    }

    // ── dist/metadata.json: { project_name, tag, version, commit } ───
    let version: String = match read_optional_json(&dest.join("metadata.json")) {
        Some(v) => v
            .get("version")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| inputs.version_hint.to_string()),
        None => inputs.version_hint.to_string(),
    };

    // ── Per-file walk of <dest>/** ───────────────────────────────────
    // Use the report's recorded hashes when available (the harness
    // already hashed every artifact it discovered; re-hashing here
    // would waste cycles). Index by BASENAME — `ArtifactRow::name`
    // is the basename for `dist/...` files (matches the harness's
    // hash-map key convention) BUT the `relative_path` field is the
    // full relative path. Strip to basename for the lookup so a
    // hypothetical row whose `name` contained a directory prefix
    // still finds its file.
    let report_by_basename: HashMap<String, &anodizer_core::ArtifactRow> = report
        .artifacts
        .iter()
        .map(|a| {
            let key = Path::new(&a.name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(a.name.as_str())
                .to_string();
            (key, a)
        })
        .collect();

    let mut entries: Vec<PreservedArtifact> = Vec::new();
    collect_preserved_entries(dest, dest, &report_by_basename, &mut entries)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let ctx = PreservedDistContext {
        artifacts: entries,
        targets,
        version,
        commit: report.commit.clone(),
    };
    let json =
        serde_json::to_string_pretty(&ctx).context("serializing PreservedDistContext to JSON")?;

    // Atomic write: stage to `.tmp` then rename so a mid-write death
    // (OOM, SIGKILL, runner timeout) never leaves a truncated
    // context.json that a Phase-2 reader would silently mis-deserialize
    // into `Default::default()`-shaped values. Mirrors the pattern in
    // `commands/release/split.rs::run_split`.
    let ctx_path = dest.join("context.json");
    let tmp_path = ctx_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)
        .with_context(|| format!("writing context.json tmp to {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &ctx_path).with_context(|| {
        format!(
            "atomically renaming {} -> {}",
            tmp_path.display(),
            ctx_path.display()
        )
    })?;
    Ok(())
}

/// Read an optional JSON sibling file. Returns:
/// - `Some(v)` when the file exists and parses cleanly.
/// - `None` when the file is missing OR present but malformed (warn
///   on the latter so the regression is loud, but don't kill the
///   manifest write).
///
/// Drops `Path::exists()` in favor of match-on-`NotFound` to avoid the
/// TOCTOU race where another process deletes the file between the
/// `exists()` check and the `read`.
fn read_optional_json(path: &Path) -> Option<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!(
                    "warn: preserved-dist {} present but malformed ({}); \
                     proceeding with harness-supplied defaults",
                    path.display(),
                    e
                );
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!(
                "warn: preserved-dist {} unreadable ({}); proceeding with \
                 harness-supplied defaults",
                path.display(),
                e
            );
            None
        }
    }
}

fn collect_preserved_entries(
    root: &Path,
    dir: &Path,
    report_by_basename: &HashMap<String, &anodizer_core::ArtifactRow>,
    out: &mut Vec<PreservedArtifact>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading preserved-dist dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_preserved_entries(root, &path, report_by_basename, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip context.json itself — we're writing it; it shouldn't
        // describe itself (the chicken-and-egg would force a re-hash
        // anyway). The atomic `.tmp` sibling also lives here mid-
        // write; skip that too so a concurrent enumerator doesn't
        // see a half-formed entry.
        if name == "context.json" || name == "context.json.tmp" {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let (sha256, size) = if let Some(row) = report_by_basename.get(name.as_str())
            && let Some(hash) = row.hash.as_ref()
        {
            (hash.clone(), row.size_bytes)
        } else {
            // Fall back to a fresh hash — file is present in the
            // preserved tree but wasn't surfaced by the harness's
            // discover walk (or had drifted/missing hash). Better to
            // ship a complete manifest than skip the entry.
            hash_file_streaming(&path)?
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

/// Stream a file through SHA-256 in 64 KiB chunks. Returns
/// `("sha256:<hex>", byte_count)`. Mirrors
/// `anodizer_core::hashing::hash_file_with`'s shape (read → update →
/// finalize), with a larger buffer (64 KiB vs 8 KiB) since this is
/// occasionally called on multi-MB raw binaries that aren't in the
/// report's hash map.
///
/// Why not reuse [`anodizer_core::hashing::sha256_file`]: it returns
/// just the hex digest, but the preserved-manifest entry needs the
/// `size` too. Wrapping it would need a separate `fs::metadata` round-
/// trip; doing both in one streaming pass costs one file open instead
/// of two.
fn hash_file_streaming(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path)
        .with_context(|| format!("opening preserved artifact {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading preserved artifact {}", path.display()))?;
        if n == 0 {
            break;
        }
        Digest::update(&mut hasher, &buf[..n]);
        total += n as u64;
    }
    Ok((format!("sha256:{:x}", hasher.finalize()), total))
}

/// Remove the preserved-dist tree after drift detection. Best-effort —
/// IO failures are warned rather than propagated so a stale preserved
/// tree never blocks the determinism report from landing. The
/// determinism check's exit code already encodes the drift; an
/// operator who needs to investigate can `rm -rf` the path manually.
pub(super) fn remove_preserved_on_drift(dest: &Path) {
    match std::fs::remove_dir_all(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!(
                "warn: failed to remove preserved-dist `{}` after drift detection: {}",
                dest.display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::{ArtifactRow, DeterminismReport};
    use tempfile::TempDir;

    fn empty_report(commit: &str) -> DeterminismReport {
        DeterminismReport {
            schema_version: 1,
            anodize_version: "test".into(),
            commit: commit.into(),
            commit_timestamp: 1_715_000_000,
            runs: 2,
            stages_under_test: vec![],
            allowlist: anodizer_core::AllowList::default(),
            artifacts: vec![],
            drift: vec![],
            drift_count: 0,
        }
    }

    /// FIX #1 regression test: `report_by_basename` MUST key on the
    /// file basename, not the relative path. A previous iteration
    /// keyed on `ArtifactRow::name` directly; when `name` was a
    /// relative path (e.g. `dist/foo.tar.gz`) the lookup missed and
    /// the context wrote a freshly-hashed value instead of the
    /// harness's recorded hash.
    ///
    /// The assertion: write a preserved file, register a report row
    /// with a relative-path `name`, mutate the preserved bytes AFTER
    /// the report was recorded, then write context.json and confirm
    /// the manifest carries the REPORT's hash (not the mutated
    /// bytes' fresh hash).
    #[test]
    fn write_context_prefers_report_hash_over_fresh_rehash() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        // Original artifact + its real hash, recorded into the
        // report. ArtifactRow.name uses the basename convention
        // matching what the harness emits.
        std::fs::write(dest.join("foo.tar.gz"), b"original-bytes").unwrap();
        let recorded_hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"original-bytes");
            format!("sha256:{:x}", h.finalize())
        };
        let mut report = empty_report("deadbeef");
        report.artifacts.push(ArtifactRow {
            // Use a RELATIVE-PATH name to exercise the basename-
            // stripping codepath that fix #1 introduced.
            name: "dist/foo.tar.gz".into(),
            path: "dist/foo.tar.gz".into(),
            size_bytes: b"original-bytes".len() as u64,
            stage: "archive".into(),
            deterministic: true,
            nondeterministic_reason: None,
            hash: Some(recorded_hash.clone()),
            hashes: vec![],
        });

        // Mutate the preserved bytes AFTER the report was recorded.
        // If context.json re-hashes from disk, it'll record the
        // mutated hash and the assertion below will fail.
        std::fs::write(dest.join("foo.tar.gz"), b"mutated-bytes-after-record").unwrap();

        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "",
            },
        )
        .expect("write_preserved_dist_context");

        let ctx_bytes = std::fs::read(dest.join("context.json")).unwrap();
        let ctx: PreservedDistContext = serde_json::from_slice(&ctx_bytes).unwrap();
        // The preserved-dist contains foo.tar.gz; the manifest must
        // record the REPORT's hash (`recorded_hash`), not a fresh hash
        // of the mutated bytes.
        let entry = ctx
            .artifacts
            .iter()
            .find(|a| a.name == "foo.tar.gz")
            .expect("manifest must include foo.tar.gz");
        assert_eq!(
            entry.sha256, recorded_hash,
            "context.json must prefer the report's hash over re-hashing disk bytes"
        );
    }

    /// FIX #2 regression test: when artifacts.json carries no
    /// `target` entries (or is missing), `targets` falls back to the
    /// harness's `--targets=<csv>` list so the manifest's `targets`
    /// field is non-empty for production runs.
    #[test]
    fn targets_falls_back_to_harness_targets_when_artifacts_json_lacks_them() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        // No artifacts.json at all → fallback to harness_targets.
        let report = empty_report("c0ffee");
        let harness_targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: Some(&harness_targets),
                version_hint: "0.0.0-fixture",
            },
        )
        .unwrap();
        let ctx: PreservedDistContext =
            serde_json::from_slice(&std::fs::read(dest.join("context.json")).unwrap()).unwrap();
        assert_eq!(
            ctx.targets,
            vec![
                "aarch64-unknown-linux-gnu".to_string(),
                "x86_64-unknown-linux-gnu".to_string()
            ],
            "harness_targets must populate `targets` when artifacts.json is missing"
        );
        assert_eq!(
            ctx.version, "0.0.0-fixture",
            "version_hint must populate `version` when metadata.json is missing"
        );
    }

    /// FIX #5 regression test: a malformed sibling JSON must not
    /// abort the manifest write. The function warns and falls back
    /// to harness-supplied defaults.
    #[test]
    fn malformed_sibling_json_falls_back_to_defaults() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        std::fs::write(dest.join("artifacts.json"), b"{not valid json").unwrap();
        std::fs::write(dest.join("metadata.json"), b"also not valid").unwrap();
        let report = empty_report("badf00d");
        let harness_targets = vec!["x86_64-pc-windows-msvc".to_string()];
        // Must NOT error; must produce a context.json with the
        // harness-supplied fallbacks.
        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: Some(&harness_targets),
                version_hint: "1.2.3-snapshot",
            },
        )
        .expect("malformed sibling JSON must not abort the manifest write");
        let ctx: PreservedDistContext =
            serde_json::from_slice(&std::fs::read(dest.join("context.json")).unwrap()).unwrap();
        assert_eq!(ctx.targets, vec!["x86_64-pc-windows-msvc".to_string()]);
        assert_eq!(ctx.version, "1.2.3-snapshot");
    }

    /// FIX #9 regression test: the write must be atomic. After a
    /// successful call there is no `context.json.tmp` sibling — the
    /// rename moved the staged file into place.
    #[test]
    fn write_context_is_atomic_no_tmp_left_behind() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        let report = empty_report("a1b2c3d");
        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "",
            },
        )
        .unwrap();
        assert!(dest.join("context.json").exists());
        assert!(
            !dest.join("context.json.tmp").exists(),
            "atomic write must rename the .tmp away on success"
        );
    }

    /// FIX #10 regression test: the streaming hasher matches the
    /// canonical `sha256:<hex>` shape and reports the correct byte
    /// count for a >64 KiB file (exercises the read loop's
    /// multi-chunk path).
    #[test]
    fn hash_file_streaming_handles_multi_chunk_files() {
        let tmp = TempDir::new().unwrap();
        // 64 KiB + 1 byte → forces a second read iteration.
        let body = vec![0xAB_u8; 64 * 1024 + 1];
        let p = tmp.path().join("big.bin");
        std::fs::write(&p, &body).unwrap();
        let (sha, size) = hash_file_streaming(&p).unwrap();
        assert_eq!(size, body.len() as u64);
        assert!(sha.starts_with("sha256:"));
        // Spot-check against a freshly-computed digest.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&body);
        assert_eq!(sha, format!("sha256:{:x}", h.finalize()));
    }
}
