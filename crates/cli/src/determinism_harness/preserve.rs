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
//! The publish-only pipeline consumes the resulting tree directly,
//! eliminating the need to recompile every target after the harness
//! has already verified byte-stable output.
//!
//! ## Why a separate module
//!
//! `artifacts.rs` owns per-run *discovery / hashing / dump-prune* — work
//! that runs inside the harness loop. Preserve-dist is end-of-loop work
//! with a different lifecycle (one-shot, runs only on the
//! green-with-flag-set path). Keeping the two concerns split keeps
//! `artifacts.rs` focused and makes the preserve-dist surface easier to
//! reason about as an integration boundary with the publish-only path.

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
/// size: Option<u64>` so a reader that already speaks `SplitContext`
/// can deserialize a `PreservedDistContext` cleanly (extra fields
/// ignored, missing fields default to `None`). The reverse direction
/// works too: deserializing a `SplitArtifact`-shaped entry as a
/// `PreservedArtifact` requires `sha256` / `size` to be present, which
/// they are when written by this module.
///
/// We deliberately do NOT reuse `SplitArtifact` directly: the harness
/// runs as a subprocess of `anodize release` and never instantiates the
/// in-process `Context::artifacts` registry, so it has no `ArtifactKind`
/// / `crate_name` / `metadata` to populate. Replicating just the fields
/// we can populate keeps `context.json` honest about what the harness
/// observed.
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
/// `targets`, `version`, `commit`. The publish-only pipeline reads
/// this file to rehydrate `ctx.artifacts` + the per-target matrix
/// before running the sign + publish stages.
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
/// Run before the per-iteration worktree is destroyed so the preserved
/// bytes survive the harness loop's teardown.
/// Bail before wiping a `dest` that looks like real user data rather
/// than a preserve target.
///
/// Safe-to-wipe cases (return `Ok`):
/// - `dest` does not exist (first run).
/// - `dest` is an empty directory.
/// - `dest` already carries a preserved-dist manifest
///   (`context.json` / `artifacts.json`) — a re-preserve into the same
///   target is intended and idempotent.
///
/// Everything else (a non-empty directory with no manifest, or a path
/// that exists but isn't a directory) is treated as data the operator
/// did not mean to destroy.
fn guard_preserve_dest(dest: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("inspecting preserve-dist dest {}", dest.display()));
        }
    };
    if !meta.is_dir() {
        anyhow::bail!(
            "refusing to overwrite --preserve-dist target {}: it exists and is not a directory. \
             Point --preserve-dist at an empty or non-existent directory.",
            dest.display()
        );
    }
    let mut entries = std::fs::read_dir(dest)
        .with_context(|| format!("reading preserve-dist dest {}", dest.display()))?
        .peekable();
    if entries.peek().is_none() {
        return Ok(()); // empty dir — safe
    }
    // Non-empty: only proceed if it's plainly a prior preserve target.
    if dest.join("context.json").exists() || dest.join("artifacts.json").exists() {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to wipe --preserve-dist target {}: directory is non-empty and does not look like \
         a preserved-dist tree (no context.json / artifacts.json). Point --preserve-dist at an empty \
         or non-existent directory to avoid accidental data loss.",
        dest.display()
    )
}

pub(super) fn preserve_dist_tree(worktree_path: &Path, dest: &Path) -> Result<()> {
    let src = worktree_path.join("dist");
    // Refuse to wipe a non-empty `dest` that isn't recognizably a prior
    // preserve target. `--preserve-dist=<dir>` feeds straight into the
    // unconditional `remove_dir_all` below, so a fat-fingered path (a
    // source tree, a home dir) would otherwise be silently destroyed.
    // An empty / non-existent dest is the normal case; a dest that
    // already carries a preserved-dist manifest is a re-run we may
    // overwrite.
    guard_preserve_dest(dest)?;
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
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("reading entry in {}", src.display()))?;
                let name = entry.file_name();
                let src_path = entry.path();
                let dst_path = dest.join(&name);
                let ft = entry
                    .file_type()
                    .with_context(|| format!("stat {}", src_path.display()))?;
                if ft.is_dir() {
                    copy_dir_recursive(&src_path, &dst_path).with_context(|| {
                        format!("copying {} -> {}", src_path.display(), dst_path.display())
                    })?;
                } else {
                    std::fs::copy(&src_path, &dst_path).with_context(|| {
                        format!("copying {} -> {}", src_path.display(), dst_path.display())
                    })?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading source dir {}", src.display()));
        }
    }
    Ok(())
}

/// Subdirectory under `<dest>/` where raw build outputs land after
/// `preserve_raw_binaries` mirrors them out of the worktree's
/// `target/<triple>/release/` tree. Single source of truth so the
/// manifest rewrite, the disk copy, and downstream consumers all agree
/// on the path prefix.
///
/// Underscore-prefixed (non-hidden) so `actions/upload-artifact@v4`
/// includes it without `include-hidden-files: true`, while still being
/// distinctive enough that a user's archive template writing to
/// `dist/bin/**` won't collide.
pub(crate) const PRESERVED_BIN_SUBDIR: &str = "_preserved-bin";

/// Mirror raw cargo build outputs into the preserved tree so the
/// publish-only `SignStage` can re-sign them under production keys.
///
/// `preserve_dist_tree` copies `<worktree>/dist/**`, which excludes raw
/// build outputs living at `<worktree>/.det-tmp/target/<triple>/release/<file>`
/// (the harness's CARGO_TARGET_DIR override). Those files are what
/// `binary_signs:` operates on, so without preservation publish-only's
/// `SignStage` would either skip them silently or crash on missing files.
///
/// This helper reads the harness child's `dist/artifacts.json` (already
/// copied verbatim into `<dest>/artifacts.json` by `preserve_dist_tree`),
/// walks every Binary-shaped or staticlib/cdylib/cdylib-style entry
/// (`Binary`, `UploadableBinary`, `Library`, `Header`, `CArchive`,
/// `CShared`, `Wasm`), copies `<worktree>/<orig_path>` to
/// `<dest>/_preserved-bin/<triple>/<basename>`, and rewrites the manifest
/// entry's `path` to `dist/_preserved-bin/<triple>/<basename>` so the
/// publish-only loader (`load_artifacts_from_manifest`) re-anchors it
/// onto the preserved tree via its existing `dist/`-prefix strip.
///
/// `UniversalBinary` is deliberately excluded from the kind list:
/// `stage-build`'s universal step writes the lipo'd output into
/// `dist/<crate>_darwin_all/<binname>` (already under `dist/`), so
/// `preserve_dist_tree` already preserves it. Re-rewriting its manifest
/// path here would orphan the original under `dist/<crate>_darwin_all/`
/// and double-preserve the bytes.
///
/// Collision guard: two artifacts sharing `(triple, basename)` would
/// silently overwrite each other under
/// `<dest>/_preserved-bin/<triple>/<basename>`, and the manifest
/// rewrite would point both entries at the same path. We bail with an
/// explanatory error instead, suggesting the user disambiguate via
/// `builds[].binary` in their config.
///
/// Idempotent / tolerant on inputs (missing artifacts.json, missing
/// `target` field, or absent source file degrade to a no-op rather
/// than aborting the preserve) — the publish-only path validates
/// rehydrated artifacts and surfaces a more actionable error if a
/// gap was tolerated here. The collision guard is the one hard error
/// because there is no actionable downstream surface for it.
///
/// Atomic-rename caveat: the staged `.json.tmp` -> `artifacts.json`
/// rename assumes the harness owns the dist tree exclusively during
/// the preserve phase. Concurrent readers would Windows-fail the
/// rename with a sharing violation; in production the harness loop
/// runs single-threaded against `dest` before the publish-only path
/// touches it, so this is safe.
pub(super) fn preserve_raw_binaries(worktree_path: &Path, dest: &Path) -> Result<()> {
    let manifest_path = dest.join("artifacts.json");
    let bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "reading {} for raw-binary preservation",
                    manifest_path.display()
                )
            });
        }
    };
    let mut manifest: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warn: preserved-dist {} present but malformed ({}); skipping raw-binary preservation",
                manifest_path.display(),
                e
            );
            return Ok(());
        }
    };
    let Some(entries) = manifest.as_array_mut() else {
        return Ok(());
    };

    // Track `(triple, basename) -> source_path` so a second entry with
    // the same key bails loudly instead of silently overwriting the
    // first copy.
    let mut copied: HashMap<(String, String), String> = HashMap::new();
    let mut rewrote_any = false;
    for entry in entries.iter_mut() {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        if !is_preservable_raw_kind(obj.get("kind").and_then(|k| k.as_str())) {
            continue;
        }
        let Some(orig_path) = obj.get("path").and_then(|p| p.as_str()).map(str::to_string) else {
            continue;
        };
        let triple = match obj.get("target").and_then(|t| t.as_str()) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };
        let basename = match std::path::Path::new(&orig_path).file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        let src = worktree_path.join(&orig_path);
        if !src.is_file() {
            continue;
        }
        let dst_rel = std::path::PathBuf::from(PRESERVED_BIN_SUBDIR)
            .join(&triple)
            .join(&basename);
        let dst_abs = dest.join(&dst_rel);
        let key = (triple.clone(), basename.clone());
        if let Some(prior_src) = copied.get(&key) {
            anyhow::bail!(
                "preserve_raw_binaries: two artifacts share (target={}, basename={}); \
                 the second copy would silently overwrite the first under {}. \
                 First source: {}; second source: {}. \
                 Disambiguate via the `builds[].binary` name in your config.",
                triple,
                basename,
                dst_abs.display(),
                prior_src,
                src.display()
            );
        }
        if let Some(parent) = dst_abs.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating preserved bin dir {}", parent.display()))?;
        }
        std::fs::copy(&src, &dst_abs).with_context(|| {
            format!(
                "copying raw binary {} -> {}",
                src.display(),
                dst_abs.display()
            )
        })?;
        copied.insert(key, src.display().to_string());
        // Rewrite the manifest path so publish-only's loader resolves it
        // under the preserved tree. The `dist/` prefix lets
        // `load_artifacts_from_manifest`'s existing strip-and-rejoin
        // logic re-anchor onto whatever dist root publish-only is
        // consuming (flat or per-crate).
        let rewritten = format!("dist/{}", dst_rel.to_string_lossy().replace('\\', "/"));
        obj.insert("path".to_string(), serde_json::Value::String(rewritten));
        rewrote_any = true;
    }

    if !rewrote_any {
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&manifest)
        .context("re-serializing artifacts.json after raw-binary path rewrite")?;
    let tmp_path = manifest_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)
        .with_context(|| format!("writing rewritten manifest tmp to {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &manifest_path).with_context(|| {
        format!(
            "atomically renaming {} -> {}",
            tmp_path.display(),
            manifest_path.display()
        )
    })?;
    Ok(())
}

/// Curated set of `ArtifactKind`s whose payload lives under
/// `target/<triple>/release/<file>` and therefore needs the
/// preserve-raw-binaries mirror to survive worktree teardown.
///
/// Excludes `UniversalBinary` (the lipo step writes its output under
/// `dist/<crate>_darwin_all/` which `preserve_dist_tree` already
/// preserves verbatim — re-rewriting would orphan-and-double-preserve).
/// Excludes packaged / installer / metadata kinds (they live under
/// `dist/` from the start).
fn is_preservable_raw_kind(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "binary"
                | "uploadable_binary"
                | "library"
                | "header"
                | "c_archive"
                | "c_shared"
                | "wasm"
        )
    )
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
/// Write is atomic via stage-to-`.tmp` + rename so a mid-write SIGKILL
/// never leaves a truncated `context.json` for a downstream reader to
/// silently mis-deserialize.
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
    // would waste cycles). Index by `ArtifactRow::name`, which is the
    // dist-root-relative path (forward-slash normalized, `dist/` prefix
    // stripped). `collect_preserved_entries` computes the same relative
    // path from `dest` and uses it as the lookup key so multi-arch
    // same-basename files are distinguished.
    let report_by_rel_path: HashMap<String, &anodizer_core::ArtifactRow> = report
        .artifacts
        .iter()
        .map(|a| (a.name.clone(), a))
        .collect();

    let mut entries: Vec<PreservedArtifact> = Vec::new();
    collect_preserved_entries(dest, dest, &report_by_rel_path, &mut entries)?;
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
    // context.json that a publish-only reader would silently
    // mis-deserialize into `Default::default()`-shaped values.
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
    report_by_rel_path: &HashMap<String, &anodizer_core::ArtifactRow>,
    out: &mut Vec<PreservedArtifact>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading preserved-dist dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_preserved_entries(root, &path, report_by_rel_path, out)?;
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
        //
        // Also skip the sibling harness manifests `artifacts.json` and
        // `metadata.json`: these are pipeline-internal metadata, not
        // shippable artifacts. The action's post-harness rename step
        // labels them per-shard (`artifacts-<shard>.json`) so that
        // `actions/download-artifact merge-multiple: true` does not
        // collide when fanning 4 shards back into one `dist/`. If we
        // record them here under the un-suffixed name, the rename
        // leaves dangling path references that `hash_verify_preserved_dist`
        // bails on (`opening preserved artifact ./dist/artifacts.json:
        // No such file or directory`). The publish-only path
        // discovers the renamed manifests directly via
        // `discover_artifacts_manifests` and does not need them in the
        // hash-verify set.
        if matches!(
            name.as_str(),
            "context.json"
                | "context.json.tmp"
                | "artifacts.json"
                | "artifacts.json.tmp"
                | "metadata.json"
                | "metadata.json.tmp"
        ) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // Lookup key matches `hash_artifacts`'s map key: dist-root-relative
        // path (the `rel` already is relative to `root` which is `dest`,
        // mirroring `hash_artifacts` stripping the `dist/` prefix).
        let (sha256, size) = if let Some(row) = report_by_rel_path.get(rel.as_str())
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

    /// Regression: `collect_preserved_entries` must look up report rows by
    /// the same relative-path key that `hash_artifacts` uses as the map key
    /// (dist-root-relative, forward-slash-normalized). A flat file
    /// `foo.tar.gz` placed directly under `dest` has rel `"foo.tar.gz"`, so
    /// `ArtifactRow.name` must also be `"foo.tar.gz"` (no `dist/` prefix —
    /// that prefix is stripped by `hash_artifacts`). If the lookup misses,
    /// context.json re-hashes from disk and this test's post-mutation
    /// assertion fails.
    #[test]
    fn write_context_prefers_report_hash_over_fresh_rehash() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        std::fs::write(dest.join("foo.tar.gz"), b"original-bytes").unwrap();
        let recorded_hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"original-bytes");
            format!("sha256:{:x}", h.finalize())
        };
        let mut report = empty_report("deadbeef");
        report.artifacts.push(ArtifactRow {
            // Name matches the dist-root-relative key `hash_artifacts`
            // would produce for `dist/foo.tar.gz` (i.e. "foo.tar.gz").
            name: "foo.tar.gz".into(),
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

    /// When artifacts.json carries no `target` entries (or is missing),
    /// `targets` falls back to the harness's `--targets=<csv>` list so
    /// the manifest's `targets` field is non-empty for production runs.
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

    /// A malformed sibling JSON must not abort the manifest write —
    /// the function warns and falls back to harness-supplied defaults
    /// so a corrupted sibling can't take down the preserve.
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

    /// The write must be atomic. After a successful call there is no
    /// `context.json.tmp` sibling — the rename moved the staged file
    /// into place, so a partial mid-write `.tmp` can't be mistaken
    /// for the canonical manifest by a downstream reader.
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

    /// Regression test: the preserved-dist manifest MUST NOT list
    /// the sibling harness manifests (`artifacts.json`,
    /// `metadata.json`) as preserved artifacts. The action's post-
    /// harness rename step labels them per-shard so multi-shard
    /// `download-artifact merge-multiple: true` does not collide;
    /// recording them under the un-suffixed name leaves dangling
    /// references that the publish-only hash-verify chokes on
    /// (`opening preserved artifact ./dist/artifacts.json: No such
    /// file or directory`). They are pipeline metadata, not
    /// shippable artifacts; publish-only discovers them directly via
    /// `discover_artifacts_manifests`.
    #[test]
    fn context_excludes_harness_sidecar_manifests() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        // Plant the three harness sidecars + one real artifact so
        // the test asserts both "sidecars excluded" and "real
        // artifacts still included" rather than just "no entries".
        std::fs::write(dest.join("artifacts.json"), b"[]").unwrap();
        std::fs::write(dest.join("metadata.json"), b"{}").unwrap();
        std::fs::write(dest.join("foo.tar.gz"), b"real artifact bytes").unwrap();
        let report = empty_report("c0ffee");
        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "0.0.0-fixture",
            },
        )
        .unwrap();
        let ctx: PreservedDistContext =
            serde_json::from_slice(&std::fs::read(dest.join("context.json")).unwrap()).unwrap();
        let names: Vec<&str> = ctx.artifacts.iter().map(|a| a.name.as_str()).collect();
        assert!(
            !names.contains(&"artifacts.json"),
            "artifacts.json must not appear as a preserved artifact (would dangle after rename): {names:?}"
        );
        assert!(
            !names.contains(&"metadata.json"),
            "metadata.json must not appear as a preserved artifact (would dangle after rename): {names:?}"
        );
        assert!(
            names.contains(&"foo.tar.gz"),
            "real artifacts must still be preserved: {names:?}"
        );
    }

    /// Raw cargo binaries live outside `dist/` in the harness worktree
    /// (under `.det-tmp/target/<triple>/release/`) and would be lost
    /// when the worktree is dropped. `preserve_raw_binaries` mirrors
    /// them into `<dest>/_preserved-bin/<triple>/<basename>` and rewrites
    /// the manifest path so publish-only's `SignStage` resolves them
    /// under the preserved tree.
    #[test]
    fn preserve_raw_binaries_copies_under_preserved_bin_subdir_and_rewrites_manifest() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let triple = "x86_64-unknown-linux-gnu";
        let bin_rel = format!(".det-tmp/target/{triple}/release/cfgd");
        let bin_src = worktree.path().join(&bin_rel);
        std::fs::create_dir_all(bin_src.parent().unwrap()).unwrap();
        std::fs::write(&bin_src, b"raw-binary-bytes").unwrap();

        std::fs::write(
            dest.path().join("artifacts.json"),
            serde_json::to_string_pretty(&serde_json::json!([
                {
                    "kind": "binary",
                    "name": "cfgd",
                    "path": bin_rel,
                    "target": triple,
                    "crate_name": "cfgd",
                    "metadata": {}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        preserve_raw_binaries(worktree.path(), dest.path())
            .expect("preserve_raw_binaries must succeed");

        let copied = dest
            .path()
            .join(PRESERVED_BIN_SUBDIR)
            .join(triple)
            .join("cfgd");
        assert!(
            copied.is_file(),
            "raw binary must land at <dest>/{PRESERVED_BIN_SUBDIR}/<triple>/<name>; \
             got missing {}",
            copied.display()
        );
        assert_eq!(std::fs::read(&copied).unwrap(), b"raw-binary-bytes");

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dest.path().join("artifacts.json")).unwrap())
                .unwrap();
        let entry = &manifest.as_array().unwrap()[0];
        assert_eq!(
            entry["path"].as_str().unwrap(),
            format!("dist/{PRESERVED_BIN_SUBDIR}/{triple}/cfgd"),
            "manifest path must be rewritten to dist/{PRESERVED_BIN_SUBDIR}/<triple>/<name> so \
             load_artifacts_from_manifest re-anchors onto the preserved tree"
        );
    }

    /// A non-Binary manifest entry next to a Binary entry must not be
    /// rewritten — the path-rewrite is scoped to the curated raw-build-
    /// output kinds.
    #[test]
    fn preserve_raw_binaries_leaves_archive_paths_untouched() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let triple = "aarch64-apple-darwin";
        let bin_rel = format!(".det-tmp/target/{triple}/release/cfgd");
        std::fs::create_dir_all(
            worktree
                .path()
                .join(format!(".det-tmp/target/{triple}/release")),
        )
        .unwrap();
        std::fs::write(worktree.path().join(&bin_rel), b"bin").unwrap();

        std::fs::write(
            dest.path().join("artifacts.json"),
            serde_json::to_string_pretty(&serde_json::json!([
                {
                    "kind": "archive",
                    "name": "cfgd_darwin_arm64.tar.gz",
                    "path": "dist/cfgd_darwin_arm64.tar.gz",
                    "target": triple,
                    "crate_name": "cfgd",
                    "metadata": {}
                },
                {
                    "kind": "binary",
                    "name": "cfgd",
                    "path": bin_rel,
                    "target": triple,
                    "crate_name": "cfgd",
                    "metadata": {}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        preserve_raw_binaries(worktree.path(), dest.path()).unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dest.path().join("artifacts.json")).unwrap())
                .unwrap();
        let arr = manifest.as_array().unwrap();
        let archive = arr.iter().find(|e| e["kind"] == "archive").unwrap();
        assert_eq!(
            archive["path"].as_str().unwrap(),
            "dist/cfgd_darwin_arm64.tar.gz",
            "archive entry must remain untouched"
        );
        let binary = arr.iter().find(|e| e["kind"] == "binary").unwrap();
        assert_eq!(
            binary["path"].as_str().unwrap(),
            format!("dist/{PRESERVED_BIN_SUBDIR}/{triple}/cfgd"),
            "binary entry must be rewritten"
        );
    }

    /// Absent `<dest>/artifacts.json` is a tolerated no-op (the harness
    /// child pipeline may have skipped artifact production altogether
    /// for a fixture run).
    #[test]
    fn preserve_raw_binaries_no_op_when_manifest_absent() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        preserve_raw_binaries(worktree.path(), dest.path())
            .expect("missing artifacts.json must not error");
    }

    /// Two entries sharing `(target, basename)` would silently
    /// overwrite each other in `<dest>/<PRESERVED_BIN_SUBDIR>/<triple>/<basename>`
    /// and the manifest rewrite would point both at the same path. The
    /// function must bail with an error naming both source paths and
    /// suggesting the `builds[].binary` disambiguation knob.
    #[test]
    fn preserve_raw_binaries_bails_on_basename_target_collision() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let triple = "x86_64-unknown-linux-gnu";
        // Two distinct source paths, same basename, same triple.
        let bin_rel_a = format!(".det-tmp/target/{triple}/release/cli");
        let bin_rel_b = format!("xtask-target/{triple}/release/cli");
        let src_a = worktree.path().join(&bin_rel_a);
        let src_b = worktree.path().join(&bin_rel_b);
        std::fs::create_dir_all(src_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(src_b.parent().unwrap()).unwrap();
        std::fs::write(&src_a, b"cli-from-main").unwrap();
        std::fs::write(&src_b, b"cli-from-xtask").unwrap();

        std::fs::write(
            dest.path().join("artifacts.json"),
            serde_json::to_string_pretty(&serde_json::json!([
                {
                    "kind": "binary",
                    "name": "cli",
                    "path": bin_rel_a,
                    "target": triple,
                    "crate_name": "main",
                    "metadata": {}
                },
                {
                    "kind": "binary",
                    "name": "cli",
                    "path": bin_rel_b,
                    "target": triple,
                    "crate_name": "xtask",
                    "metadata": {}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        let err = preserve_raw_binaries(worktree.path(), dest.path())
            .expect_err("collision must bail")
            .to_string();
        assert!(
            err.contains(triple) && err.contains("cli") && err.contains("builds[].binary"),
            "error must name the colliding triple+basename and suggest a fix; got: {err}"
        );
        // Source files must reference both paths so the operator can
        // tell which build configs produced the collision.
        assert!(
            err.contains(&src_a.display().to_string()),
            "error must name the first source path; got: {err}"
        );
        assert!(
            err.contains(&src_b.display().to_string()),
            "error must name the second source path; got: {err}"
        );
    }

    /// `UniversalBinary` entries live UNDER `dist/<crate>_darwin_all/`
    /// (written by `stage-build`'s universal step) and are already
    /// preserved verbatim by `preserve_dist_tree`. The raw-binary
    /// preserver must NOT rewrite their manifest paths — doing so
    /// would orphan the original copy under `dist/<crate>_darwin_all/`
    /// and double-preserve the bytes under `<PRESERVED_BIN_SUBDIR>/`.
    #[test]
    fn preserve_raw_binaries_skips_universal_binary_to_avoid_double_preservation() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        // The universal artifact already lives under dist/, so the
        // manifest's `path` is `dist/<crate>_darwin_all/<binname>` and
        // there's no separate raw-build source to copy from.
        let uni_path = "dist/cfgd_darwin_all/cfgd";

        std::fs::write(
            dest.path().join("artifacts.json"),
            serde_json::to_string_pretty(&serde_json::json!([
                {
                    "kind": "universal_binary",
                    "name": "cfgd",
                    "path": uni_path,
                    "target": "darwin-universal",
                    "crate_name": "cfgd",
                    "metadata": {}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        preserve_raw_binaries(worktree.path(), dest.path())
            .expect("preserve must succeed even with only a UniversalBinary entry");

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dest.path().join("artifacts.json")).unwrap())
                .unwrap();
        let entry = &manifest.as_array().unwrap()[0];
        assert_eq!(
            entry["path"].as_str().unwrap(),
            uni_path,
            "UniversalBinary manifest path must remain untouched (preserve_dist_tree owns it)"
        );
        // The mirror subdir must NOT exist — no copy happened for the
        // universal entry.
        assert!(
            !dest
                .path()
                .join(PRESERVED_BIN_SUBDIR)
                .join("darwin-universal")
                .exists(),
            "no <PRESERVED_BIN_SUBDIR>/darwin-universal copy may exist; the universal \
             entry must round-trip via preserve_dist_tree only"
        );
    }

    /// Library / Header / CArchive / CShared / Wasm outputs live at
    /// `target/<triple>/release/<file>` exactly like raw binaries (see
    /// `stage-build/src/run.rs` for the Wasm + Library branches). They
    /// must therefore travel through the same preserve path so
    /// publish-only's `SignStage` and downstream consumers see them.
    #[test]
    fn preserve_raw_binaries_extends_kind_set_to_library_header_carchive_cshared_wasm() {
        let worktree = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let triple = "x86_64-unknown-linux-gnu";

        // One artifact per extended kind, each at a distinct source
        // file so the collision guard doesn't fire.
        let cases = [
            ("library", "libfoo.so"),
            ("header", "foo.h"),
            ("c_archive", "libfoo.a"),
            ("c_shared", "libfoo_shared.so"),
            ("wasm", "foo.wasm"),
        ];

        let mut entries: Vec<serde_json::Value> = Vec::new();
        for (kind, basename) in cases.iter() {
            let rel = format!(".det-tmp/target/{triple}/release/{basename}");
            let abs = worktree.path().join(&rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, format!("bytes-of-{basename}")).unwrap();
            entries.push(serde_json::json!({
                "kind": kind,
                "name": basename,
                "path": rel,
                "target": triple,
                "crate_name": "foo",
                "metadata": {}
            }));
        }
        std::fs::write(
            dest.path().join("artifacts.json"),
            serde_json::to_string_pretty(&serde_json::json!(entries)).unwrap(),
        )
        .unwrap();

        preserve_raw_binaries(worktree.path(), dest.path())
            .expect("preserve_raw_binaries must accept all extended kinds");

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dest.path().join("artifacts.json")).unwrap())
                .unwrap();
        let arr = manifest.as_array().unwrap();
        for (kind, basename) in cases.iter() {
            let copied = dest
                .path()
                .join(PRESERVED_BIN_SUBDIR)
                .join(triple)
                .join(basename);
            assert!(
                copied.is_file(),
                "{kind} artifact `{basename}` must be mirrored under \
                 <PRESERVED_BIN_SUBDIR>/<triple>/; missing {}",
                copied.display()
            );
            let entry = arr.iter().find(|e| e["kind"] == *kind).unwrap();
            assert_eq!(
                entry["path"].as_str().unwrap(),
                format!("dist/{PRESERVED_BIN_SUBDIR}/{triple}/{basename}"),
                "{kind} entry must have its manifest path rewritten"
            );
        }
    }

    /// `preserve_dist_tree` copies the full `dist/` tree verbatim, including
    /// `dist/makeself/<id>/<arch>/` staging directories. Per-arch files with
    /// the same basename (e.g. `anodizer`) are keyed by their dist-root-
    /// relative path in the hash map, so they are distinct entries and the
    /// manifest carries correct hash/path pairs. The band-aid that filtered
    /// `makeself/` is gone; the root-cause key collision is fixed instead.
    #[test]
    fn preserve_dist_tree_includes_makeself_per_arch_dirs() {
        let src_root = TempDir::new().unwrap();
        let dest_root = TempDir::new().unwrap();
        let dist = src_root.path().join("dist");
        // Shippable .run lives under dist/linux/ — must be preserved.
        std::fs::create_dir_all(dist.join("linux")).unwrap();
        std::fs::write(
            dist.join("linux")
                .join("anodizer-0.3.0-linux-amd64-installer.run"),
            b"shippable .run bytes",
        )
        .unwrap();
        // Two per-arch staging dirs sharing a basename — both must be preserved.
        for arch in &["linux_amd64", "linux_arm64"] {
            let stage_dir = dist.join("makeself").join("default").join(arch);
            std::fs::create_dir_all(&stage_dir).unwrap();
            std::fs::write(stage_dir.join("anodizer"), format!("staging-{}", arch)).unwrap();
            std::fs::write(stage_dir.join("makeself-install.sh"), b"install").unwrap();
        }

        preserve_dist_tree(src_root.path(), dest_root.path())
            .expect("preserve_dist_tree must succeed");

        assert!(
            dest_root
                .path()
                .join("linux/anodizer-0.3.0-linux-amd64-installer.run")
                .exists(),
            "shippable .run must survive preservation",
        );
        // Per-arch staging files are now preserved — the relative-path key
        // prevents the hash-map collision that the old band-aid worked around.
        assert!(
            dest_root
                .path()
                .join("makeself/default/linux_amd64/anodizer")
                .exists(),
            "makeself/linux_amd64/anodizer must be preserved",
        );
        assert!(
            dest_root
                .path()
                .join("makeself/default/linux_arm64/anodizer")
                .exists(),
            "makeself/linux_arm64/anodizer must be preserved",
        );
    }

    /// End-to-end multi-arch contract: threads a same-basename multi-arch
    /// fixture through the full pipeline (discover_artifacts → hash_artifacts
    /// → build a DeterminismReport carrying those rows → preserve_dist_tree →
    /// write_preserved_dist_context → read context.json back) and asserts
    /// that BOTH per-arch entries land in the manifest carrying the hashes
    /// the harness recorded (not freshly re-hashed against disk). Catches a
    /// key-contract drift between the hashing layer and the preservation
    /// lookup that the piece-wise tests cannot.
    #[test]
    fn multi_arch_round_trip_preserves_distinct_hashes_from_report() {
        use super::super::artifacts::{discover_artifacts, hash_artifacts};

        let wt = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        // Two arch dirs, both containing a file named `anodizer` with
        // distinct bytes. Plus a top-level dist artifact for good measure.
        let dist = wt.path().join("dist");
        std::fs::create_dir_all(dist.join("makeself/default/linux_amd64")).unwrap();
        std::fs::create_dir_all(dist.join("makeself/default/linux_arm64")).unwrap();
        std::fs::write(
            dist.join("makeself/default/linux_amd64/anodizer"),
            b"amd64-bytes-original",
        )
        .unwrap();
        std::fs::write(
            dist.join("makeself/default/linux_arm64/anodizer"),
            b"arm64-bytes-original",
        )
        .unwrap();

        // Drive the real pipeline: discover → hash.
        let paths = discover_artifacts(wt.path()).unwrap();
        let hash_map = hash_artifacts(wt.path(), &paths).unwrap();
        let amd64_key = "makeself/default/linux_amd64/anodizer";
        let arm64_key = "makeself/default/linux_arm64/anodizer";
        let amd64_hash = hash_map[amd64_key].hash.clone();
        let arm64_hash = hash_map[arm64_key].hash.clone();
        assert_ne!(
            amd64_hash, arm64_hash,
            "fixture must produce distinct hashes"
        );

        // Synthesize the report `Harness::build_report` would emit: one row
        // per map entry, `name` = the map key, `path` = the dist-prefixed
        // relative path.
        let mut report = empty_report("e2e-commit");
        for (key, info) in &hash_map {
            report.artifacts.push(ArtifactRow {
                name: key.clone(),
                path: format!("dist/{}", key),
                size_bytes: info.size_bytes,
                stage: info.stage.clone(),
                deterministic: true,
                nondeterministic_reason: None,
                hash: Some(info.hash.clone()),
                hashes: vec![],
            });
        }

        // Run the preservation pipeline against the real wt → dest copy.
        preserve_dist_tree(wt.path(), dest.path()).expect("preserve_dist_tree");

        // Tamper with the preserved arm64 bytes AFTER the report was built.
        // If `write_preserved_dist_context` re-hashes from disk instead of
        // pulling from the report, the recorded hash will diverge from
        // `arm64_hash` and the assertion below will fail.
        std::fs::write(
            dest.path().join("makeself/default/linux_arm64/anodizer"),
            b"arm64-bytes-MUTATED",
        )
        .unwrap();

        write_preserved_dist_context(
            dest.path(),
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "0.0.0-fixture",
            },
        )
        .expect("write_preserved_dist_context");

        // Read back and assert both arch entries carry the REPORT's hash.
        let ctx: PreservedDistContext =
            serde_json::from_slice(&std::fs::read(dest.path().join("context.json")).unwrap())
                .unwrap();
        let amd64_entry = ctx
            .artifacts
            .iter()
            .find(|a| a.path == amd64_key)
            .unwrap_or_else(|| panic!("amd64 entry missing in {:?}", ctx.artifacts));
        let arm64_entry = ctx
            .artifacts
            .iter()
            .find(|a| a.path == arm64_key)
            .unwrap_or_else(|| panic!("arm64 entry missing in {:?}", ctx.artifacts));
        assert_eq!(
            amd64_entry.sha256, amd64_hash,
            "amd64 entry must carry the harness-recorded hash"
        );
        assert_eq!(
            arm64_entry.sha256, arm64_hash,
            "arm64 entry must carry the harness-recorded hash even after \
             the bytes on disk were tampered with — proves the lookup hit \
             the report instead of re-hashing"
        );
    }

    /// The streaming hasher matches the canonical `sha256:<hex>` shape
    /// and reports the correct byte count for a file larger than the
    /// 64 KiB read buffer — exercises the multi-chunk read loop so a
    /// regression that loses bytes between iterations is caught.
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

    // ── Per-crate subdir layout tests ─────────────────────────────────────────

    /// When the harness computes an effective_preserve_dest of `<base>/<crate>/`
    /// and calls write_preserved_dist_context with that dest, context.json
    /// must land in `<base>/<crate>/context.json` (not at the flat root).
    /// This test simulates that call — the subdir computation is in mod.rs;
    /// here we verify the write itself lands in whatever dest is passed.
    #[test]
    fn write_context_in_subdir_when_called_with_subdir_dest() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        let subdir = dest.join("my-crate");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("foo.tar.gz"), b"artifact").unwrap();
        let report = empty_report("abc123");

        write_preserved_dist_context(
            &subdir,
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "1.0.0",
            },
        )
        .expect("write_preserved_dist_context into subdir");

        let subdir_context = subdir.join("context.json");
        assert!(
            subdir_context.exists(),
            "context.json must be written into the subdir passed as dest"
        );
        assert!(
            !dest.join("context.json").exists(),
            "context.json must NOT appear at the flat root when dest is a subdir"
        );

        let ctx: PreservedDistContext =
            serde_json::from_slice(&std::fs::read(&subdir_context).unwrap()).unwrap();
        assert_eq!(ctx.version, "1.0.0");
        assert_eq!(ctx.commit, "abc123");
    }

    /// When called with the flat base dest (no crate_name subdir), context.json
    /// lands at the flat root as before.
    #[test]
    fn write_context_flat_when_called_with_base_dest() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path();
        std::fs::write(dest.join("foo.tar.gz"), b"artifact").unwrap();
        let report = empty_report("deadbeef");

        write_preserved_dist_context(
            dest,
            ContextInputs {
                report: &report,
                harness_targets: None,
                version_hint: "2.0.0",
            },
        )
        .expect("write_preserved_dist_context at flat root");

        assert!(
            dest.join("context.json").exists(),
            "context.json must be at flat root when dest is the base"
        );
    }

    #[test]
    fn preserve_bails_on_non_empty_non_manifest_dest() {
        let worktree = TempDir::new().unwrap();
        std::fs::create_dir_all(worktree.path().join("dist")).unwrap();
        let dest = TempDir::new().unwrap();
        // Operator fat-fingered --preserve-dist at a real directory full
        // of data with no preserved-dist manifest.
        std::fs::write(dest.path().join("important.txt"), b"do not delete").unwrap();
        std::fs::create_dir_all(dest.path().join("src")).unwrap();

        let err = preserve_dist_tree(worktree.path(), dest.path())
            .expect_err("must refuse to wipe a non-empty, non-manifest dest");
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to wipe") && msg.contains("empty or non-existent"),
            "error must be actionable: {msg}"
        );
        assert!(
            dest.path().join("important.txt").exists(),
            "operator data must survive the bail"
        );
    }

    #[test]
    fn preserve_allows_empty_dest() {
        let worktree = TempDir::new().unwrap();
        std::fs::create_dir_all(worktree.path().join("dist")).unwrap();
        let dest = TempDir::new().unwrap(); // exists but empty
        preserve_dist_tree(worktree.path(), dest.path())
            .expect("empty dest is the normal case and must be allowed");
    }

    #[test]
    fn preserve_allows_prior_preserve_target() {
        let worktree = TempDir::new().unwrap();
        std::fs::create_dir_all(worktree.path().join("dist")).unwrap();
        let dest = TempDir::new().unwrap();
        // A prior preserve target carries the manifest; re-preserving is
        // intended and idempotent, so it must be allowed to overwrite.
        std::fs::write(dest.path().join("context.json"), b"{}").unwrap();
        std::fs::write(dest.path().join("stale.tar.gz"), b"old").unwrap();
        preserve_dist_tree(worktree.path(), dest.path())
            .expect("a prior preserve target must be overwritable");
        assert!(
            !dest.path().join("stale.tar.gz").exists(),
            "stale bytes from the prior preserve must be cleared"
        );
    }
}
