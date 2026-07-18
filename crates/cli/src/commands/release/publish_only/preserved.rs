use super::*;
use anodizer_core::log::StageLogger;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Minimal `PreservedDistContext` deserializer. We re-declare the
/// shape here rather than depending on `determinism_harness::preserve`
/// to keep this module decoupled from harness internals — the
/// schema (artifacts + targets + version + commit) is the
/// load-bearing contract, not the producer module.
///
/// `#[serde(default)]` on every field so a partially-written
/// `context.json` from a buggy producer doesn't kill the load — the
/// downstream artifact-load step is the real gate. Missing fields
/// degrade gracefully (empty targets / version / commit).
#[derive(serde::Deserialize, Debug, Default, Clone)]
pub(super) struct PreservedDistContext {
    #[serde(default)]
    pub(super) artifacts: Vec<PreservedArtifact>,
    #[serde(default)]
    pub(super) targets: Vec<String>,
    #[serde(default)]
    pub(super) version: String,
    #[serde(default)]
    pub(super) commit: String,
}

/// Per-artifact entry in `context.json`: name + relative path,
/// SHA256 (in `sha256:<hex>` form), and byte size. Consumed by
/// [`hash_verify_preserved_dist`] to cross-check on-disk bytes
/// against the determinism record before re-signing.
#[derive(serde::Deserialize, Debug, Default, Clone)]
pub(super) struct PreservedArtifact {
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) path: String,
    #[serde(default)]
    pub(super) sha256: String,
    #[serde(default)]
    pub(super) size: u64,
}

/// Find every `<base>` and `<stem>-*.json` entry at the dist root
/// (non-recursive), where `base` is the canonical `<stem>.json` basename
/// (an `anodizer_core::dist` const, e.g. [`anodizer_core::dist::CONTEXT_JSON`]).
/// `*.tmp` siblings are skipped — those are leftover atomic-write scratch
/// files from the harness's rename-into-place writer and never represent a
/// committed manifest. Returns the matching paths sorted by filename for
/// reproducible output.
///
/// Single source of truth for the two sharded-manifest families
/// (`context.json` / `context-<shard>.json` and `artifacts.json` /
/// `artifacts-<shard>.json`).
pub(super) fn discover_sharded_manifests(dist: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: reading dist directory {} to discover {} manifest(s)",
            dist.display(),
            base,
        )
    })?;
    // `base` is the canonical `<name>.json` basename; sharded variants
    // swap the extension for `-<shard>.json`.
    let stem = base.strip_suffix(".json").unwrap_or(base);
    let exact = base;
    let prefix = format!("{stem}-");
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "publish-only: reading directory entry under {}",
                dist.display()
            )
        })?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        // Skip the .tmp file the harness's atomic-rename writer may
        // have left behind on a crash mid-write — never represents a
        // committed manifest. Applies uniformly to both manifest
        // families.
        if name.ends_with(".tmp") {
            continue;
        }
        if name == exact || (name.starts_with(&prefix) && name.ends_with(".json")) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Walk `dist/` for every `context.json` and `context-*.json` entry at
/// the dist root (non-recursive). Returns the parsed contexts paired
/// with their source paths, sorted by filename for reproducible output.
/// Empty result is an error — `publish-only` cannot proceed without at
/// least one manifest pinning the preserved commit.
pub(super) fn discover_preserved_contexts(
    dist: &Path,
) -> Result<Vec<(PathBuf, PreservedDistContext)>> {
    let found = discover_sharded_manifests(dist, anodizer_core::dist::CONTEXT_JSON)?;
    if found.is_empty() {
        anyhow::bail!(
            "publish-only: no context.json (or context-<shard>.json) found at {}. \
             Run `anodize check determinism --preserve-dist=<dist-dir>` on a green \
             determinism check first, or use `anodize publish` (no sign step) if \
             you only need the publisher pass.",
            dist.display()
        );
    }
    let mut out: Vec<(PathBuf, PreservedDistContext)> = Vec::with_capacity(found.len());
    for path in found {
        let parsed = load_preserved_context(&path)?;
        out.push((path, parsed));
    }
    Ok(out)
}

/// Walk `dist/` for every `artifacts.json` and `artifacts-*.json` entry
/// at the dist root (non-recursive). Returns paths sorted by filename
/// for reproducible output. May return an empty vec when neither the
/// legacy nor any sharded manifest is present — callers decide whether
/// that's fatal.
pub(super) fn discover_artifacts_manifests(dist: &Path) -> Result<Vec<PathBuf>> {
    discover_sharded_manifests(dist, anodizer_core::dist::ARTIFACTS_JSON)
}

/// Detect the upload-artifact merge-collision symptom: both
/// `<base>.json` AND any `<base>-*.json` exist side-by-side at dist
/// root. That shouldn't happen under either layout — the legacy
/// single-shard mode emits ONLY `<base>.json`, the sharded mode
/// renames into `<base>-<shard>.json` so `merge-multiple: true`
/// won't collide. Both present means an upstream workflow change
/// (or a hand-edited dist) merged shards' un-suffixed manifests
/// over each other and one shard "won" — the surviving file is
/// silently a single shard's view, not the union.
pub(super) fn check_no_unsuffixed_suffixed_collision(dist: &Path, base: &str) -> Result<()> {
    let unsuffixed = dist.join(format!("{base}.json"));
    if !unsuffixed.is_file() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: scanning {} for sharded {} manifests",
            dist.display(),
            base,
        )
    })?;
    let prefix = format!("{base}-");
    let mut sharded: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(".tmp") {
            continue;
        }
        if name.starts_with(&prefix) && name.ends_with(".json") {
            sharded.push(entry.path());
        }
    }
    if !sharded.is_empty() {
        sharded.sort();
        let sharded_display = sharded
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<?>")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "publish-only: both {base}.json AND sharded {base}-*.json ({sharded_display}) \
             exist at {dist}. This indicates upload-artifact merged shards' \
             un-suffixed {base}.json files over each other before they were \
             properly suffixed — the surviving {base}.json is only one shard's view. \
             Either delete the un-suffixed {base}.json (if the sharded files are \
             authoritative) or delete the sharded files (legacy single-shard mode).",
            base = base,
            sharded_display = sharded_display,
            dist = dist.display(),
        );
    }
    Ok(())
}

/// Fold N per-shard `PreservedDistContext` entries into a single view.
/// Semantics:
/// - `artifacts` — concatenated in shard-name (path) order; duplicates
///   are preserved (a duplicate path across shards is a workflow bug
///   worth surfacing downstream rather than silently collapsing).
/// - `targets` — deduped + sorted; the union across all shards.
/// - `version` / `commit` — taken from the first non-empty entry; ALL
///   non-empty values across shards must agree, else this fails closed.
///   An empty `commit` on the merged view is also fatal — without it
///   we cannot prove the preserved bytes match the current release.
///
/// Cross-checks live inside this fold so the merge contract has one
/// home: any caller of `merge_preserved_contexts` receives a view that
/// has already been validated end-to-end. Splitting "merge" and
/// "validate" across two call sites is the bug magnet this prevents.
pub(super) fn merge_preserved_contexts(
    contexts: &[(PathBuf, PreservedDistContext)],
) -> Result<PreservedDistContext> {
    use std::collections::BTreeSet;
    let mut merged = PreservedDistContext::default();
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for (_, c) in contexts {
        if merged.version.is_empty() && !c.version.is_empty() {
            merged.version = c.version.clone();
        }
        if merged.commit.is_empty() && !c.commit.is_empty() {
            merged.commit = c.commit.clone();
        }
        for t in &c.targets {
            targets.insert(t.clone());
        }
        for a in &c.artifacts {
            merged.artifacts.push(PreservedArtifact {
                name: a.name.clone(),
                path: a.path.clone(),
                sha256: a.sha256.clone(),
                size: a.size,
            });
        }
    }
    merged.targets = targets.into_iter().collect();

    // ── Cross-checks (fail closed) ────────────────────────────────────
    // Empty merged `commit` means NO shard recorded one. Re-signing
    // without a commit anchor breaks the determinism guarantee: we
    // can't prove the preserved bytes match the current release.
    if merged.commit.is_empty() {
        anyhow::bail!(
            "publish-only: no context manifest carried a `commit` field. Cannot verify the \
             preserved bytes match the current release; re-run \
             `anodize check determinism --preserve-dist=...` with a producer that \
             records the commit SHA."
        );
    }
    // Every shard's `commit` MUST agree with the merged value. A
    // mismatch means two shards were preserved from two different
    // release attempts — re-signing across that mix would publish
    // bytes whose determinism guarantee is split across commits.
    for (path, ctx_entry) in contexts {
        if !ctx_entry.commit.is_empty() && ctx_entry.commit != merged.commit {
            anyhow::bail!(
                "publish-only: shard manifest {} records commit {} but the merged set is \
                 anchored at {}. A multi-shard preserved dist must come from a single \
                 release attempt; mixing bytes from different commits would publish \
                 signatures whose determinism-verified state is split.",
                path.display(),
                short_commit_str(&ctx_entry.commit),
                short_commit_str(&merged.commit),
            );
        }
    }
    // Same gate for `version`: a shard mismatch means two different
    // release attempts' contexts were folded together.
    for (path, ctx_entry) in contexts {
        if !ctx_entry.version.is_empty() && ctx_entry.version != merged.version {
            anyhow::bail!(
                "publish-only: shard manifest {} records version {} but the merged set is \
                 anchored at {}. A multi-shard preserved dist must come from a single \
                 release attempt; mixing bytes across versions would publish \
                 signatures whose determinism-verified state is split.",
                path.display(),
                ctx_entry.version,
                merged.version,
            );
        }
    }

    Ok(merged)
}

pub(super) fn load_preserved_context(path: &Path) -> Result<PreservedDistContext> {
    if !path.exists() {
        // The recovery hint uses a literal `<dist-dir>` placeholder
        // rather than interpolating `path.parent()` because the parent
        // for a relative `dist/context.json` would be `.` (or empty),
        // producing the misleading "--preserve-dist=." in the error.
        // A literal placeholder is unambiguous.
        anyhow::bail!(
            "publish-only: missing {}. Run `anodize check determinism \
             --preserve-dist=<dist-dir>` on a green determinism check first, or use \
             `anodize publish` (no sign step) if you only need the publisher pass.",
            path.display(),
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("publish-only: read {}", path.display()))?;
    let ctx: PreservedDistContext = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "publish-only: parse {} as PreservedDistContext",
            path.display()
        )
    })?;
    Ok(ctx)
}

/// Filename suffixes whose bytes the publish-only path will replace
/// via `strip_ephemeral_signatures` + the head `SignStage` re-sign.
/// hash-verifying them across shards is meaningless: cosign's ECDSA
/// nonce makes per-shard signatures of identical content diverge by
/// design, and the bytes are discarded before the production keys
/// re-sign anyway. Verifying them would block multi-shard releases on
/// signatures whose mismatch is an architectural feature, not a
/// corruption signal.
///
/// Stays narrow on purpose: `.sig` (cosign / gpg detached signatures),
/// `.asc` (gpg armored signatures), `.pem` (cosign signing certs).
/// Any future ephemeral-output kind should be added here AND the
/// `strip_ephemeral_signatures` filter that consumes it.
pub(super) const EPHEMERAL_SIGNATURE_SUFFIXES: &[&str] = &[".sig", ".asc", ".pem"];

pub(super) fn is_ephemeral_signature_path(path: &str) -> bool {
    EPHEMERAL_SIGNATURE_SUFFIXES
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

/// Cross-check that every artifact recorded in the preserved
/// `context.json` matches the on-disk bytes under `dist_root`. Pins
/// the determinism-check → publish-only safety invariant: the bytes
/// shipped MUST be the bytes the harness verified. Closes the
/// silent-corruption window between `upload-artifact` /
/// `download-artifact` in the CI fan-out.
///
/// Skips ephemeral signature/certificate paths (`.sig`, `.asc`,
/// `.pem`): they vary per shard (cosign ECDSA nonce) and are stripped
/// then re-signed by [`strip_ephemeral_signatures`] before publish,
/// so verifying them would fail the multi-shard fan-out on signatures
/// whose mismatch is an architectural feature.
pub(super) fn hash_verify_preserved_dist(
    ctx: &PreservedDistContext,
    dist_root: &Path,
) -> Result<()> {
    use std::collections::BTreeMap;

    // Group recorded hashes by relative path. The merged context carries
    // one entry per (shard, path) pair, so a cross-shard duplicate like
    // `anodizer-<ver>-source.tar.gz` (produced independently on every
    // shard) shows up once per shard with potentially differing recorded
    // bytes — git/tar/locale variance across OS runners is real and
    // shows up here. After `actions/download-artifact merge-multiple`,
    // exactly ONE shard's bytes survive on disk for any given path, so
    // the disk file must match SOME shard's claim, not all of them
    // simultaneously.
    let mut by_path: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for artifact in &ctx.artifacts {
        if is_ephemeral_signature_path(&artifact.path) {
            continue;
        }
        by_path
            .entry(artifact.path.as_str())
            .or_default()
            .push(artifact.sha256.as_str());
    }

    for (path_str, expected_hashes) in &by_path {
        let path = dist_root.join(path_str);
        let actual_hex = anodizer_core::hashing::sha256_file(&path).with_context(|| {
            format!(
                "publish-only hash-verify: hashing preserved artifact {}",
                path.display(),
            )
        })?;
        let actual = format!("sha256:{actual_hex}");

        // Tolerate bare hex OR `sha256:<hex>` on the recorded side.
        // The harness writes the prefixed form today; accepting both
        // keeps the contract loose for future producers.
        let expected_normalized: Vec<String> = expected_hashes
            .iter()
            .map(|h| {
                if h.starts_with("sha256:") {
                    (*h).to_string()
                } else {
                    format!("sha256:{h}")
                }
            })
            .collect();
        let matches_any = expected_normalized.iter().any(|e| e == &actual);

        if !matches_any {
            // Distinct expected values, deduped + sorted for a stable
            // error message that shows the operator every shard's
            // recorded view of this path.
            let mut distinct: Vec<&String> = expected_normalized.iter().collect();
            distinct.sort();
            distinct.dedup();
            let expected_list = distinct
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "publish-only hash-verify: bytes on disk diverge from every shard's recorded \
                 determinism state for {} (recorded across {} shard(s): [{}], on disk: {}). \
                 The dist tree was modified between determinism check and publish, OR no \
                 shard's preserved bytes survived `download-artifact merge-multiple` — \
                 refusing to ship.",
                path.display(),
                expected_normalized.len(),
                expected_list,
                actual,
            );
        }
    }
    Ok(())
}

/// Delete sharded `artifacts-<shard>.json` manifests at dist root after
/// the canonical un-suffixed `artifacts.json` has been re-written by
/// `run_post_pipeline`.
///
/// Scope is limited to the `artifacts` family on purpose:
/// `run_post_pipeline` re-writes the un-suffixed `artifacts.json` from
/// the merged in-memory context, which makes the per-shard
/// `artifacts-<shard>.json` files stale the instant that write lands.
/// The `context` family has no equivalent un-suffixed re-writer — only
/// the harness emits `write_preserved_dist_context`, and that only
/// produces shard-suffixed files. Cleaning `context-<shard>.json` here
/// would leave a subsequent retry with no manifest at all and trip
/// `discover_preserved_contexts`'s bail.
///
/// Best-effort: logs a warn on each remove failure but does not fail
/// the publish — by the time this is called the release has already
/// completed successfully, and a stale shard manifest only matters on
/// the next retry (where it would trip
/// `check_no_unsuffixed_suffixed_collision`).
///
/// Manual recovery: if `run_post_pipeline` succeeded and the process
/// was SIGKILL'd before this cleanup ran, `dist/` will hold both the
/// canonical `dist/artifacts.json` AND the per-shard
/// `dist/artifacts-<shard>.json` siblings. A retry would bail in
/// `check_no_unsuffixed_suffixed_collision`. Clear the shard files
/// before retry: `rm dist/artifacts-*.json`.
pub(super) fn cleanup_shard_manifests(dist: &Path, log: &StageLogger) {
    let base = "artifacts";
    let entries = match std::fs::read_dir(dist) {
        Ok(e) => e,
        Err(e) => {
            log.warn(&format!(
                "failed to read {} for shard-manifest cleanup: {} \
                 (a retry may trip the unsuffixed-vs-suffixed collision check)",
                dist.display(),
                e,
            ));
            return;
        }
    };
    let prefix = format!("{base}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name_str.starts_with(&prefix) && name_str.ends_with(".json") {
            let path = entry.path();
            if let Err(e) = std::fs::remove_file(&path) {
                log.warn(&format!(
                    "failed to remove shard manifest {}: {} \
                     (a retry may trip the unsuffixed-vs-suffixed collision check)",
                    path.display(),
                    e
                ));
            }
        }
    }
}
