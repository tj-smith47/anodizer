//! `anodize release --publish-only`: consume a `dist/` populated by
//! `anodize check determinism --preserve-dist=<path>` and run only the
//! sign + publish pipeline.
//!
//! The harness writes:
//! - `<preserved-dist>/**` — the byte-stable artifacts the determinism
//!   check just verified (archives, packages, sboms, checksums,
//!   `artifacts.json`, `metadata.json`).
//! - `<preserved-dist>/context.json` — a [`PreservedDistContext`]
//!   manifest pinning `(artifacts, targets, version, commit)`.
//!
//! This mode loads both, rehydrates `ctx.artifacts` from
//! `dist/artifacts.json` (the in-process registry shape — the manifest
//! the post-pipeline already writes), strips any leftover
//! `Signature` / `Certificate` artifacts the harness may have produced
//! with ephemeral keys, then runs an extended publish pipeline that
//! prepends `SignStage` (production-keys sign pass) ahead of the usual
//! release / publish / blob / snapcraft-publish chain.
//!
//! Idempotence: the harness skips its in-loop `SignStage` when
//! production keys are exported on the runner (`COSIGN_KEY` /
//! `GPG_PRIVATE_KEY`), so preserved-dist usually has no `.sig` /
//! `.asc` files. This module's defensive strip exists for the case
//! where that gate didn't fire (harness ran without prod keys then
//! operator brought them in later, etc.) — re-signing on top of an
//! existing signature chain would produce `*.sig.sig` chaos.
//!
//! Pipeline choice: the merge pipeline assumes raw-binary input from
//! `--split`. `--publish-only` deliberately bypasses that assumption:
//! input is the FULL artifact set (binaries + archives + packages +
//! checksums), so we run `build_publish_only_pipeline` (see
//! `crate::pipeline`), not `build_merge_pipeline`.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::git::short_commit_str;
use anodizer_core::log::StageLogger;

use super::helpers;
use crate::pipeline;

/// Names of the env vars that gate the publish-only credential
/// preflight. Documented as a single source of truth so the error
/// message and the check itself stay in lockstep.
const SIGN_ENV_VARS: &[&str] = &["COSIGN_KEY", "GPG_PRIVATE_KEY"];
const GITHUB_TOKEN_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "ANODIZER_GITHUB_TOKEN"];

/// Knobs the dispatcher hands to `publish_only::run`. Reduces the
/// number of positional arguments and lets the dispatch site speak
/// in terms of flag intent (`no_preflight`) rather than the threaded
/// `--<flag>` boolean it came from.
pub(super) struct RunOpts {
    pub dry_run: bool,
    /// `--no-preflight`: skip the credential preflight as well as the
    /// publisher-state preflight. Operator opt-out for the case
    /// where they know what they're doing and want the mid-pipeline
    /// failure to surface instead.
    pub no_preflight: bool,
}

/// `--publish-only` entry point. Wired from `commands/release/mod.rs::run`
/// after `setup_context` / git context / preflight have already run on
/// `ctx`.
pub(super) fn run(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    opts: RunOpts,
) -> Result<()> {
    log.status("running in publish-only mode (load preserved dist + sign + publish)...");

    let dist = config.dist.clone();

    // ── Pre-flight credential check ────────────────────────────────────
    // Bail BEFORE any state mutation: a credential miss this late
    // (mid-pipeline) leaves a partially-uploaded release behind with no
    // idempotent recovery. Dry-run skips so operators can preview the
    // pipeline without secrets; `--no-preflight` is the explicit
    // operator opt-out for the rare case where they want the
    // mid-pipeline failure instead.
    if opts.dry_run {
        log.verbose("(dry-run) skipping production-credential preflight");
    } else if opts.no_preflight {
        log.warn(
            "credential preflight skipped via --no-preflight; \
             missing credentials will fail mid-pipeline (no idempotent recovery)",
        );
    } else {
        preflight_credentials(|k| std::env::var(k).ok())?;
    }

    // ── Load preserved-dist context ────────────────────────────────────
    // Two manifest families live in `<dist>/`:
    //   - `artifacts.json` / `artifacts-<shard>.json`: the canonical
    //     in-process registry shape (`kind` / `target` / `metadata`),
    //     same as `anodize publish` consumes. Each shard emits its own.
    //   - `context.json` / `context-<shard>.json`: the harness's
    //     `PreservedDistContext` summary with per-artifact `sha256` +
    //     `size` recorded at preserve time. Each shard emits its own.
    //
    // The sharded release workflow uploads each shard's dist tree under
    // `dist-<shard>` and the action renames the per-shard manifests so
    // download-artifact's `merge-multiple: true` doesn't collide on
    // identically-named files. Discovery here folds them all back in.
    //
    // The legacy single-`context.json` layout (operator running locally
    // without sharding) keeps working — `discover_preserved_contexts`
    // matches both the un-suffixed and the suffixed forms.
    //
    // Detect the upload-artifact merge-collision symptom BEFORE
    // loading anything: both un-suffixed AND suffixed manifests
    // present is a workflow bug we should never silently paper over.
    check_no_unsuffixed_suffixed_collision(&dist, "context")?;
    check_no_unsuffixed_suffixed_collision(&dist, "artifacts")?;

    let preserved_contexts = discover_preserved_contexts(&dist)?;
    let preserved = merge_preserved_contexts(&preserved_contexts)?;
    let shard_count = preserved_contexts.len();

    log.status(&format!(
        "publish-only: loaded {} context manifest(s) (version={}, commit={}, targets=[{}], {} artifact(s))",
        shard_count,
        preserved.version,
        short_commit_str(&preserved.commit),
        preserved.targets.join(", "),
        preserved.artifacts.len(),
    ));

    // Pin the determinism-check → publish-only safety invariant: hash
    // every preserved artifact's bytes BEFORE the commit cross-check
    // and any registry mutation. A mismatch here means the dist tree
    // is no longer the bytes the harness verified — refuse to ship
    // rather than re-sign corrupted input.
    hash_verify_preserved_dist(&preserved, &dist)?;

    // Commit / version cross-checks across shards now live inside
    // `merge_preserved_contexts` — they're part of the merge contract,
    // not a separate post-processing step.
    let ctx_commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();
    if ctx_commit.is_empty() {
        anyhow::bail!(
            "publish-only: current release context has no resolved commit. \
             Run from a tagged commit (`git checkout {}`) before --publish-only.",
            short_commit_str(&preserved.commit),
        );
    }
    if ctx_commit != preserved.commit {
        anyhow::bail!(
            "publish-only: context manifest was preserved at commit {} but the current \
             release context resolved to commit {}. Re-signing the preserved bytes \
             under the current commit's tag would ship signatures that don't match \
             the determinism-verified state. `git checkout {}` then retry.",
            short_commit_str(&preserved.commit),
            short_commit_str(&ctx_commit),
            short_commit_str(&preserved.commit),
        );
    }

    // ── Rehydrate ctx.artifacts ────────────────────────────────────────
    // Delegates to the same loader `anodize publish` uses so the two
    // entry points stay in lockstep (one parser to maintain). Each
    // discovered manifest contributes its artifacts to the registry;
    // duplicate paths across shards indicate the matrix overlapped on
    // a target — surface as a hard error rather than silently de-dupe.
    let artifact_manifests = discover_artifacts_manifests(&dist)?;
    for manifest_path in &artifact_manifests {
        helpers::load_artifacts_from_manifest(ctx, &dist, manifest_path).with_context(|| {
            format!(
                "publish-only: failed to load {} from {}. The preserve-dist \
                 flow normally copies these from the harness's worktree post-pipeline; \
                 if any is missing the preserved dist is incomplete.",
                manifest_path.display(),
                dist.display()
            )
        })?;
    }

    log.status(&format!(
        "publish-only: rehydrated {} artifact(s) from {} artifacts manifest(s)",
        ctx.artifacts.all().len(),
        artifact_manifests.len(),
    ));

    // Binary + UniversalBinary entries in `artifacts.json` point at the
    // harness child's per-target build outputs under
    // `.det-tmp/target/<triple>/release/<bin>`, NOT under `dist/`. The
    // preserve-dist tree only captures `dist/**`, so those paths don't
    // exist on the release runner. release_uploadable_kinds() already
    // excludes both kinds from GitHub release uploads — they're raw
    // build outputs, never shipped directly — so dropping them from
    // the registry here loses nothing the downstream pipeline can
    // act on. Skipping the drop would trip detect_missing_artifact_files
    // (hard bail) and, if bypassed, crash binary_signs at cosign
    // sign-blob time when the missing path can't be opened.
    drop_stale_binary_artifacts(ctx, log);

    // Fail closed on duplicate artifact paths across the merged
    // manifests. Sharded determinism matrices partition the target
    // set across shards, so a duplicate `path` after the union means
    // two shards both claimed the same artifact — either the matrix
    // overlapped or someone hand-edited a manifest. Re-signing
    // duplicate entries would produce double-emit confusion in
    // SignStage / ReleaseStage (the same file uploaded twice, or
    // worse, the same sidecar overwritten in race).
    detect_duplicate_artifact_paths(ctx)?;

    // Filesystem vs manifest cross-check: every artifact path the
    // manifest references must actually exist on disk. Missing files
    // means the preserved dist is incomplete — running through to
    // SignStage would fail with a less actionable error from
    // cosign/gpg, so we surface it here with a manifest-shaped
    // diagnostic instead. We do NOT flag unreferenced files (the
    // dist tree carries metadata.json, harness logs, etc. that aren't
    // in the artifacts manifest).
    detect_missing_artifact_files(ctx, &dist)?;

    // ── Strip ephemeral signatures / certificates ──────────────────────
    // Defensive: the harness skips SignStage when production keys are
    // exported on the runner, so preserved-dist usually has no `.sig`
    // / `.asc` files. But re-signing on top of an existing chain (e.g.
    // operator ran the harness without prod keys, then brought them
    // in) would emit `*.sig.sig` / `*.pem.sig` — corrupt checksums
    // and confuse downstream verifiers. Strip up-front so `SignStage`
    // always sees a clean input registry.
    strip_ephemeral_signatures(ctx, log);

    // ── Run the extended publish pipeline ──────────────────────────────
    // `build_publish_only_pipeline` prepends `SignStage` ahead of the
    // usual release / publish / blob / snapcraft-publish chain — the
    // head SignStage is the production-keys re-sign pass that overlays
    // shippable signatures on the byte-stable preserved archives.
    // Distinct from `build_publish_pipeline` (consumed by `anodize
    // publish`) which does NOT prepend SignStage; conflating them
    // would silently introduce a new credential requirement to
    // `anodize publish`.
    let p = pipeline::build_publish_only_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        super::run_post_pipeline(ctx, config, opts.dry_run, log)?;

        // run_post_pipeline writes the canonical un-suffixed
        // artifacts.json from the merged registry. The per-shard
        // manifests (artifacts-*.json, context-*.json) that fed the
        // merge are no longer load-bearing, and their continued
        // presence next to the new un-suffixed file would trip
        // check_no_unsuffixed_suffixed_collision on a retry. Delete
        // them so a second invocation (operator-driven workflow rerun)
        // sees a clean canonical layout. Best-effort: by the time this
        // runs the release has already completed successfully, so a
        // remove failure is logged but never propagated.
        cleanup_shard_manifests(&dist, log);
    }

    // Same gate as `release` / `--merge`: required-publisher failures
    // must surface as a non-zero exit even though per-publisher
    // failures are non-fatal inside the pipeline body.
    if result.is_ok() {
        super::gate_required_failures(ctx)?;
    }

    result
}

/// Pre-flight credential check. Fires BEFORE any state mutation so a
/// credential miss doesn't leave a partially-uploaded release behind.
///
/// Required: a GitHub-shaped token (release stage needs to upload
/// assets / create the release) AND at least one of the production
/// signing keys (sign stage re-signs the preserved archives). Other
/// publisher credentials (chocolatey api key, AUR ssh key, etc.) are
/// per-publisher and surface at dispatch time — the pre-flight
/// publisher-state check (separate code path, runs before this branch
/// in `commands/release/mod.rs`) already validates them.
///
/// Pragmatic intentionally: the user is expected to drive
/// publish-only from CI where the secrets are exported into env
/// once. Re-deriving "which env vars matter per publisher" lives in
/// each stage's own preflight — duplicating it here would diverge.
///
/// **Env injection** (`env`): callers pass a closure that resolves
/// env-var names to values. The production caller delegates to
/// `std::env::var`; unit tests pass a pure closure so test execution
/// doesn't race with sibling tests on shared process env. This
/// mirrors how `stage-sign::helpers::should_sign_artifact` is
/// independently testable.
fn preflight_credentials(env: impl Fn(&str) -> Option<String>) -> Result<()> {
    let token_present = GITHUB_TOKEN_ENV_VARS
        .iter()
        .any(|v| env(v).map(|s| !s.is_empty()).unwrap_or(false));
    let sign_key_present = SIGN_ENV_VARS
        .iter()
        .any(|v| env(v).map(|s| !s.is_empty()).unwrap_or(false));

    if !token_present {
        anyhow::bail!(
            "publish-only: missing release token. Set one of {} before running --publish-only \
             (or pass --dry-run to preview without secrets).",
            GITHUB_TOKEN_ENV_VARS.join(" / "),
        );
    }
    if !sign_key_present {
        anyhow::bail!(
            "publish-only: missing production signing key. Set at least one of {} before \
             running --publish-only (or pass --dry-run to preview without secrets). \
             The harness's ephemeral signatures are NOT shippable — this mode exists \
             to overlay production signatures on the byte-stable artifacts.",
            SIGN_ENV_VARS.join(" / "),
        );
    }
    Ok(())
}

/// Strip `Binary` / `UniversalBinary` entries from `ctx.artifacts`.
/// The harness child registers these via `ctx.artifacts.add()` with
/// paths under `.det-tmp/target/<triple>/release/<bin>` (relativized
/// against the worktree cwd). Preserve-dist only captures `dist/**`,
/// so on the release runner those paths don't exist. Two failure
/// modes if we leave them in place:
///   1. `detect_missing_artifact_files` walks every registered path
///      and bails with a "preserved dist incomplete" diagnostic before
///      SignStage ever runs.
///   2. If that check is bypassed, `binary_signs` invokes cosign
///      sign-blob on the missing path and crashes with a less actionable
///      error.
///
/// `release_uploadable_kinds()` explicitly excludes both kinds — they're
/// raw build outputs, not uploaded to the GitHub release — so dropping
/// them from the registry here loses nothing the publish path actually
/// uses. Symmetric with [`strip_ephemeral_signatures`]: registry first,
/// then we'd delete on disk except the paths don't exist on this runner
/// to begin with (and we don't want to try `.det-tmp/...` removal from
/// the release runner anyway).
fn drop_stale_binary_artifacts(ctx: &mut Context, log: &StageLogger) {
    use anodizer_core::artifact::ArtifactKind;
    let stale_binary_paths: Vec<std::path::PathBuf> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary))
        .map(|a| a.path.clone())
        .collect();
    if stale_binary_paths.is_empty() {
        return;
    }
    let n = stale_binary_paths.len();
    ctx.artifacts.remove_by_paths(&stale_binary_paths);
    log.status(&format!(
        "publish-only: dropped {n} Binary/UniversalBinary artifact(s) from registry \
         (raw build outputs not in preserved-dist; release_uploadable_kinds excludes \
         them too — nothing downstream consumes these)"
    ));
    // Binary-level signing has nothing to sign once we've dropped the
    // raw binaries. Surface that to the operator instead of silently
    // skipping — otherwise a consumer with `binary_signs:` configured
    // would assume binary-level cosign blobs were produced.
    if !ctx.config.binary_signs.is_empty() {
        log.warn(
            "publish-only: binary_signs is configured but raw binaries are not preserved \
             into dist/ by the determinism harness — binary-level signatures will NOT be \
             produced in --publish-only mode. To ship signed binaries, either configure \
             archive-level signs:, or sign at consumer-side (e.g. cosign verify-blob \
             against the binaries inside the released archive).",
        );
    }
}

/// Strip `Signature` / `Certificate` artifacts the harness may have
/// left behind (with ephemeral keys). `SignStage`'s own
/// `should_sign_artifact` already filters Signature/Certificate kinds
/// out of the `all`/`any` artifacts set so a no-op re-sign wouldn't
/// emit `.sig.sig`, but the resulting registry would still include
/// the ephemeral artifacts — which then get UPLOADED by `ReleaseStage`.
/// We must remove them at the source.
///
/// Symmetry note: any non-signature/certificate artifact remains
/// untouched, including any `Checksum` entries — re-signing produces
/// a new signature blob but the underlying archive bytes (which the
/// checksums are computed from) are unchanged, so the checksums
/// still match. `ChecksumStage` is intentionally not in the publish
/// pipeline for the same reason: nothing to recompute.
fn strip_ephemeral_signatures(ctx: &mut Context, log: &StageLogger) {
    use anodizer_core::artifact::ArtifactKind;
    let stale_paths: Vec<std::path::PathBuf> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| matches!(a.kind, ArtifactKind::Signature | ArtifactKind::Certificate))
        .map(|a| a.path.clone())
        .collect();
    if stale_paths.is_empty() {
        return;
    }
    let count = stale_paths.len();
    log.status(&format!(
        "publish-only: stripping {count} ephemeral signature/certificate artifact(s) before re-sign"
    ));
    // Registry FIRST, then disk. If the process is signaled between
    // the two steps, a retry sees a consistent state: the registry
    // has no dangling entries that point at files the next run is
    // about to find on disk anyway (SignStage will overwrite them
    // cleanly). The reverse order leaves a window where the file is
    // gone but the registry still references it — a follow-up
    // ArtifactKind::Signature lookup would then find a phantom.
    ctx.artifacts.remove_by_paths(&stale_paths);
    // Now delete on-disk files so the next sign-stage doesn't see
    // a leftover `.sig` next to its target and produce a `*.sig.sig`
    // through the user's own sign-args template (which typically reads
    // `{{ .Signature }} = {{ .Artifact }}.sig`).
    let mut disk_removed = 0usize;
    for p in &stale_paths {
        match std::fs::remove_file(p) {
            Ok(()) => disk_removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log.warn(&format!(
                "publish-only: failed to delete stale signature {}: {} \
                 (continuing; SignStage will overwrite or fail loudly)",
                p.display(),
                e
            )),
        }
    }
    // Positive success signal so the operator sees the strip happened
    // (counter-balances the lone "stripping N..." line above which
    // could otherwise look like the work stalled). Reports both the
    // registry-side removal count (always equal to `count`) and the
    // disk-side count (may be lower if a sig was already absent on
    // disk — registry entries can outlive their files when the
    // post-pipeline runs partial writes).
    log.status(&format!(
        "publish-only: stripped {count} ephemeral signature artifact(s) from registry \
         ({disk_removed} also deleted from disk)"
    ));
}

/// Walk `ctx.artifacts` grouped by `path` and fail if any path appears
/// more than once. Called post-rehydration so a sharded matrix that
/// accidentally overlapped on a target surfaces as a hard error rather
/// than a double-publish downstream.
///
/// Thin wrapper over `commands::helpers::detect_duplicate_paths` that
/// projects the artifact iter into a path iter.
fn detect_duplicate_artifact_paths(ctx: &Context) -> Result<()> {
    crate::commands::helpers::detect_duplicate_paths(
        ctx.artifacts.all().iter().map(|a| a.path.as_path()),
    )
}

/// Walk every artifact in `ctx.artifacts` and verify its `path` exists
/// on disk under `dist/`. Thin wrapper over
/// `commands::helpers::detect_missing_files`.
fn detect_missing_artifact_files(ctx: &Context, dist: &Path) -> Result<()> {
    crate::commands::helpers::detect_missing_files(
        ctx.artifacts.all().iter().map(|a| a.path.as_path()),
        dist,
    )
}

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
struct PreservedDistContext {
    #[serde(default)]
    artifacts: Vec<PreservedArtifact>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    commit: String,
}

/// Per-artifact entry in `context.json`: name + relative path,
/// SHA256 (in `sha256:<hex>` form), and byte size. Consumed by
/// [`hash_verify_preserved_dist`] to cross-check on-disk bytes
/// against the determinism record before re-signing.
#[derive(serde::Deserialize, Debug, Default, Clone)]
struct PreservedArtifact {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    size: u64,
}

/// Find every `<base>.json` and `<base>-*.json` entry at the dist root
/// (non-recursive). `*.tmp` siblings are skipped — those are leftover
/// atomic-write scratch files from the harness's rename-into-place
/// writer and never represent a committed manifest. Returns the
/// matching paths sorted by filename for reproducible output.
///
/// Single source of truth for the two sharded-manifest families
/// (`context.json` / `context-<shard>.json` and `artifacts.json` /
/// `artifacts-<shard>.json`).
fn discover_sharded_manifests(dist: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: reading dist directory {} to discover {} manifest(s)",
            dist.display(),
            base,
        )
    })?;
    let exact = format!("{base}.json");
    let prefix = format!("{base}-");
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
fn discover_preserved_contexts(dist: &Path) -> Result<Vec<(PathBuf, PreservedDistContext)>> {
    let found = discover_sharded_manifests(dist, "context")?;
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
fn discover_artifacts_manifests(dist: &Path) -> Result<Vec<PathBuf>> {
    discover_sharded_manifests(dist, "artifacts")
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
fn check_no_unsuffixed_suffixed_collision(dist: &Path, base: &str) -> Result<()> {
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
fn merge_preserved_contexts(
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

fn load_preserved_context(path: &Path) -> Result<PreservedDistContext> {
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

/// Cross-check that every artifact recorded in the preserved
/// `context.json` matches the on-disk bytes under `dist_root`. Pins
/// the determinism-check → publish-only safety invariant: the bytes
/// shipped MUST be the bytes the harness verified. Closes the
/// silent-corruption window between `upload-artifact` /
/// `download-artifact` in the CI fan-out.
fn hash_verify_preserved_dist(ctx: &PreservedDistContext, dist_root: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    for artifact in &ctx.artifacts {
        let path = dist_root.join(&artifact.path);
        let mut file = std::fs::File::open(&path).with_context(|| {
            format!(
                "publish-only hash-verify: opening preserved artifact {}",
                path.display(),
            )
        })?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .with_context(|| format!("publish-only hash-verify: reading {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let actual_hex = format!("{:x}", hasher.finalize());
        let actual = format!("sha256:{actual_hex}");

        // Tolerate bare hex OR `sha256:<hex>` on the recorded side.
        // The harness writes the prefixed form today; accepting both
        // keeps the contract loose for future producers.
        let expected = if artifact.sha256.starts_with("sha256:") {
            artifact.sha256.clone()
        } else {
            format!("sha256:{}", artifact.sha256)
        };

        if actual != expected {
            anyhow::bail!(
                "publish-only hash-verify: bytes on disk diverge from determinism record for {} \
                 (expected {}, got {}). The dist tree may have been modified between determinism \
                 check and publish — refusing to ship.",
                path.display(),
                expected,
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
fn cleanup_shard_manifests(dist: &Path, log: &StageLogger) {
    let base = "artifacts";
    let entries = match std::fs::read_dir(dist) {
        Ok(e) => e,
        Err(e) => {
            log.warn(&format!(
                "publish-only: failed to read {} for shard-manifest cleanup: {} \
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
                    "publish-only: failed to remove shard manifest {}: {} \
                     (a retry may trip the unsuffixed-vs-suffixed collision check)",
                    path.display(),
                    e
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a closure-returning factory for an env map; tests pass it
    /// to `preflight_credentials` to drive the credential check
    /// deterministically without touching the process env.
    fn env_from(map: HashMap<&str, &str>) -> impl Fn(&str) -> Option<String> {
        let owned: HashMap<String, String> = map
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.get(k).cloned()
    }

    #[test]
    fn load_preserved_context_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_preserved_context(&tmp.path().join("context.json")).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("publish-only: missing"),
            "error should name the publish-only path; got: {msg}"
        );
        assert!(
            msg.contains("--preserve-dist"),
            "error should point at the preserve-dist flag; got: {msg}"
        );
        // The error must use the literal `<dist-dir>` placeholder, not
        // a `path.parent()` interpolation that would emit "." for
        // relative paths and confuse the operator on the recovery hint.
        assert!(
            msg.contains("<dist-dir>"),
            "error should use the literal <dist-dir> placeholder; got: {msg}"
        );
    }

    #[test]
    fn load_preserved_context_parses_minimal_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("context.json");
        std::fs::write(
            &path,
            r#"{"artifacts":[{"name":"a.tar.gz","path":"a.tar.gz","sha256":"sha256:abc","size":42}],"targets":["x86_64-unknown-linux-gnu"],"version":"0.1.0","commit":"deadbeefcafe"}"#,
        )
        .unwrap();
        let parsed = load_preserved_context(&path).unwrap();
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.commit, "deadbeefcafe");
        assert_eq!(parsed.targets, vec!["x86_64-unknown-linux-gnu"]);
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0].name, "a.tar.gz");
    }

    #[test]
    fn load_preserved_context_tolerates_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("context.json");
        std::fs::write(&path, r#"{}"#).unwrap();
        let parsed = load_preserved_context(&path).unwrap();
        assert!(parsed.artifacts.is_empty());
        assert!(parsed.targets.is_empty());
        assert_eq!(parsed.version, "");
        assert_eq!(parsed.commit, "");
    }

    #[test]
    fn preflight_credentials_bails_when_token_missing() {
        let err = preflight_credentials(|_| None).unwrap_err();
        assert!(
            format!("{err}").contains("missing release token"),
            "expected missing-token error; got: {err}"
        );
    }

    #[test]
    fn preflight_credentials_bails_when_sign_key_missing() {
        let env = env_from(HashMap::from([("GITHUB_TOKEN", "x")]));
        let err = preflight_credentials(env).unwrap_err();
        assert!(
            format!("{err}").contains("missing production signing key"),
            "expected missing-sign-key error after token set; got: {err}"
        );
    }

    #[test]
    fn preflight_credentials_accepts_token_and_cosign_key() {
        let env = env_from(HashMap::from([("GITHUB_TOKEN", "x"), ("COSIGN_KEY", "y")]));
        preflight_credentials(env).expect("token + cosign should preflight clean");
    }

    #[test]
    fn preflight_credentials_accepts_anodizer_github_token_alias() {
        // The token gate honors both `GITHUB_TOKEN` and
        // `ANODIZER_GITHUB_TOKEN` — verifying the alias avoids a
        // silent regression if someone narrows the constant list.
        let env = env_from(HashMap::from([
            ("ANODIZER_GITHUB_TOKEN", "x"),
            ("GPG_PRIVATE_KEY", "y"),
        ]));
        preflight_credentials(env).expect("anodizer github token + gpg key should preflight clean");
    }

    #[test]
    fn preflight_credentials_rejects_empty_token_value() {
        // Empty-string values count as "missing" (the env-var was
        // exported but never populated). Guards against the case
        // where CI declares the secret but the upstream provider
        // returned nothing.
        let env = env_from(HashMap::from([("GITHUB_TOKEN", ""), ("COSIGN_KEY", "y")]));
        let err = preflight_credentials(env).unwrap_err();
        assert!(
            format!("{err}").contains("missing release token"),
            "empty token must be treated as missing; got: {err}"
        );
    }

    // ── discover_sharded_manifests / .tmp skip ────────────────────────

    #[test]
    fn discover_sharded_manifests_skips_tmp_siblings_uniformly() {
        // Both manifest families (`context`, `artifacts`) must skip a
        // `*.tmp` file the harness's atomic-rename writer may have
        // left mid-crash — a leftover scratch file never represents a
        // committed manifest, regardless of which base it sits next to.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("context.json.tmp"), "garbage").unwrap();
        std::fs::write(tmp.path().join("artifacts.json"), "[]").unwrap();
        std::fs::write(tmp.path().join("artifacts.json.tmp"), "garbage").unwrap();
        std::fs::write(tmp.path().join("artifacts-linux.json"), "[]").unwrap();
        std::fs::write(tmp.path().join("artifacts-linux.json.tmp"), "garbage").unwrap();

        let ctx = discover_sharded_manifests(tmp.path(), "context").unwrap();
        let names: Vec<String> = ctx
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["context.json"], "tmp siblings must be skipped");

        let arts = discover_sharded_manifests(tmp.path(), "artifacts").unwrap();
        let names: Vec<String> = arts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["artifacts-linux.json", "artifacts.json"],
            "artifacts family must also skip .tmp; got {names:?}"
        );
    }

    // ── un-suffixed + suffixed coexistence ────────────────────────────

    #[test]
    fn collision_check_errors_when_unsuffixed_and_suffixed_both_present_context() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("context-linux.json"), "{}").unwrap();
        let err = check_no_unsuffixed_suffixed_collision(tmp.path(), "context").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("context.json") && msg.contains("context-linux.json"),
            "error should name both colliding manifests; got: {msg}"
        );
        assert!(
            msg.contains("upload-artifact merged"),
            "error should name the symptom hypothesis; got: {msg}"
        );
    }

    #[test]
    fn collision_check_errors_when_unsuffixed_and_suffixed_both_present_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("artifacts.json"), "[]").unwrap();
        std::fs::write(tmp.path().join("artifacts-darwin.json"), "[]").unwrap();
        let err = check_no_unsuffixed_suffixed_collision(tmp.path(), "artifacts").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("artifacts.json") && msg.contains("artifacts-darwin.json"),
            "error should name both colliding manifests; got: {msg}"
        );
    }

    #[test]
    fn collision_check_ok_for_unsuffixed_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
        check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
            .expect("unsuffixed-only must be fine");
    }

    #[test]
    fn collision_check_ok_for_suffixed_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("context-a.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("context-b.json"), "{}").unwrap();
        check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
            .expect("suffixed-only must be fine");
    }

    #[test]
    fn collision_check_ignores_tmp_sibling_of_suffixed() {
        // A leftover `*.tmp` next to a single un-suffixed manifest
        // must NOT trip the collision check (the tmp file is harness
        // crash debris, not a real shard manifest).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("context-linux.json.tmp"), "garbage").unwrap();
        check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
            .expect(".tmp sibling must not trigger collision");
    }

    // ── merge_preserved_contexts cross-checks ─────────────────────────

    fn ctx_entry(version: &str, commit: &str) -> PreservedDistContext {
        PreservedDistContext {
            artifacts: vec![],
            targets: vec![],
            version: version.to_string(),
            commit: commit.to_string(),
        }
    }

    #[test]
    fn merge_preserved_contexts_bails_when_commit_empty_everywhere() {
        let contexts = vec![
            (PathBuf::from("context-a.json"), ctx_entry("0.1.0", "")),
            (PathBuf::from("context-b.json"), ctx_entry("0.1.0", "")),
        ];
        let err = merge_preserved_contexts(&contexts).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no context manifest carried a `commit`"),
            "expected commit-missing diagnostic; got: {msg}"
        );
    }

    #[test]
    fn merge_preserved_contexts_bails_on_commit_mismatch_across_shards() {
        let contexts = vec![
            (
                PathBuf::from("context-a.json"),
                ctx_entry("0.1.0", "deadbeefcafe"),
            ),
            (
                PathBuf::from("context-b.json"),
                ctx_entry("0.1.0", "ba5eba11feed"),
            ),
        ];
        let err = merge_preserved_contexts(&contexts).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("records commit") && msg.contains("merged set is"),
            "expected per-shard commit-mismatch diagnostic; got: {msg}"
        );
        assert!(
            msg.contains("context-b.json"),
            "diagnostic must name the dissenting shard; got: {msg}"
        );
    }

    #[test]
    fn merge_preserved_contexts_bails_on_version_mismatch_across_shards() {
        let contexts = vec![
            (
                PathBuf::from("context-a.json"),
                ctx_entry("0.1.0", "deadbeefcafe"),
            ),
            (
                PathBuf::from("context-b.json"),
                ctx_entry("0.2.0", "deadbeefcafe"),
            ),
        ];
        let err = merge_preserved_contexts(&contexts).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("records version") && msg.contains("merged set is"),
            "expected per-shard version-mismatch diagnostic; got: {msg}"
        );
        assert!(
            msg.contains("context-b.json"),
            "diagnostic must name the dissenting shard; got: {msg}"
        );
    }

    #[test]
    fn merge_preserved_contexts_accepts_consistent_shards() {
        let contexts = vec![
            (
                PathBuf::from("context-a.json"),
                ctx_entry("0.1.0", "deadbeefcafe"),
            ),
            (
                PathBuf::from("context-b.json"),
                ctx_entry("0.1.0", "deadbeefcafe"),
            ),
        ];
        let merged = merge_preserved_contexts(&contexts).expect("consistent shards must merge");
        assert_eq!(merged.commit, "deadbeefcafe");
        assert_eq!(merged.version, "0.1.0");
    }

    #[test]
    fn merge_preserved_contexts_tolerates_one_shard_with_empty_commit() {
        // Half-populated shards (some carry commit, others empty) are
        // fine: the empty entries simply don't anchor the merged
        // value. The cross-check only fires when a non-empty entry
        // disagrees.
        let contexts = vec![
            (PathBuf::from("context-a.json"), ctx_entry("0.1.0", "")),
            (
                PathBuf::from("context-b.json"),
                ctx_entry("0.1.0", "deadbeefcafe"),
            ),
        ];
        let merged = merge_preserved_contexts(&contexts).expect("mixed-empty shards must merge");
        assert_eq!(merged.commit, "deadbeefcafe");
    }

    // ── detect_duplicate_paths_in ──────────────────────────────────────

    #[test]
    fn detect_duplicate_paths_in_passes_on_unique_set() {
        let paths = [Path::new("a.tar.gz"), Path::new("b.tar.gz")];
        crate::commands::helpers::detect_duplicate_paths(paths).expect("unique paths must pass");
    }

    #[test]
    fn detect_duplicate_paths_in_flags_repeated_path() {
        let paths = [
            Path::new("a.tar.gz"),
            Path::new("b.tar.gz"),
            Path::new("a.tar.gz"),
        ];
        let err = crate::commands::helpers::detect_duplicate_paths(paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("a.tar.gz"),
            "error must name the duplicated path; got: {msg}"
        );
        assert!(
            msg.contains("(2×)"),
            "error must show the duplicate count; got: {msg}"
        );
        assert!(
            msg.contains("shards overlapped"),
            "error must name the matrix-overlap hypothesis; got: {msg}"
        );
    }

    // ── detect_missing_files_in ────────────────────────────────────────

    #[test]
    fn detect_missing_files_in_passes_when_all_present() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.tar.gz");
        std::fs::write(&a, b"x").unwrap();
        // Mix absolute (the loader's default shape) and relative paths
        // to ensure both code paths are exercised.
        std::fs::write(tmp.path().join("rel.tar.gz"), b"x").unwrap();
        let paths = [a.as_path(), Path::new("rel.tar.gz")];
        crate::commands::helpers::detect_missing_files(paths, tmp.path())
            .expect("all present must pass");
    }

    #[test]
    fn detect_missing_files_in_errors_on_absent_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.tar.gz");
        let paths = [missing.as_path()];
        let err = crate::commands::helpers::detect_missing_files(paths, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does-not-exist.tar.gz"),
            "error must name the missing file; got: {msg}"
        );
        assert!(
            msg.contains("preserved dist is incomplete"),
            "error must surface the incomplete-dist hypothesis; got: {msg}"
        );
    }

    #[test]
    fn detect_missing_files_in_errors_on_absent_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = [Path::new("rel-missing.tar.gz")];
        let err = crate::commands::helpers::detect_missing_files(paths, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rel-missing.tar.gz"),
            "error must name the missing relative file; got: {msg}"
        );
    }

    #[test]
    fn detect_missing_files_in_ignores_files_not_in_manifest() {
        // Files that exist in dist/ but are NOT in the manifest are
        // fine — the cross-check only flags MISSING references, not
        // unreferenced files (metadata.json, harness logs, etc.).
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.tar.gz");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(tmp.path().join("metadata.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("orphan.tar.gz"), b"x").unwrap();
        let paths = [a.as_path()];
        crate::commands::helpers::detect_missing_files(paths, tmp.path())
            .expect("unreferenced dist files must not trigger the check");
    }

    // ── hash_verify_preserved_dist ─────────────────────────────────────

    /// `sha256("hello world")` — pinned literal so the matching-bytes
    /// test doesn't recompute the hash via the very function under test.
    const HELLO_WORLD_SHA256: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    #[test]
    fn hash_verify_preserved_dist_accepts_matching_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hello world").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![PreservedArtifact {
                name: "hello.txt".into(),
                path: "hello.txt".into(),
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 11,
            }],
            ..PreservedDistContext::default()
        };
        hash_verify_preserved_dist(&ctx, tmp.path()).expect("matching bytes must verify clean");
    }

    #[test]
    fn hash_verify_preserved_dist_rejects_mismatched_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = "hello.txt";
        std::fs::write(tmp.path().join(rel), b"hello world").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![PreservedArtifact {
                name: rel.into(),
                path: rel.into(),
                // Wrong hash on purpose — drives the mismatch branch.
                sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .into(),
                size: 11,
            }],
            ..PreservedDistContext::default()
        };
        let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("diverge"),
            "error must surface the divergence wording; got: {msg}"
        );
        assert!(
            msg.contains(rel),
            "error must name the offending file; got: {msg}"
        );
    }

    #[test]
    fn hash_verify_preserved_dist_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![PreservedArtifact {
                name: "absent.tar.gz".into(),
                path: "absent.tar.gz".into(),
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 11,
            }],
            ..PreservedDistContext::default()
        };
        let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("opening preserved artifact"),
            "error must surface the open-failure wording; got: {msg}"
        );
        assert!(
            msg.contains("absent.tar.gz"),
            "error must name the missing file; got: {msg}"
        );
    }

    /// Cleanup must drop the stale per-shard `artifacts-<shard>.json`
    /// manifests but leave `context-<shard>.json` alone — see the
    /// function-level doc-comment on `cleanup_shard_manifests`.
    #[test]
    fn cleanup_shard_manifests_removes_only_artifacts_shards_leaves_context() {
        use anodizer_core::log::Verbosity;
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path();
        // Set up: one un-suffixed artifacts.json (the canonical), three
        // sharded artifacts-*.json, three sharded context-*.json.
        std::fs::write(dist.join("artifacts.json"), b"[]").unwrap();
        std::fs::write(dist.join("artifacts-ubuntu-latest.json"), b"[]").unwrap();
        std::fs::write(dist.join("artifacts-macos-latest.json"), b"[]").unwrap();
        std::fs::write(dist.join("artifacts-windows-x86_64.json"), b"[]").unwrap();
        std::fs::write(dist.join("context-ubuntu-latest.json"), b"{}").unwrap();
        std::fs::write(dist.join("context-macos-latest.json"), b"{}").unwrap();

        let log = StageLogger::new("test", Verbosity::Quiet);
        cleanup_shard_manifests(dist, &log);

        // Canonical artifacts.json survives.
        assert!(dist.join("artifacts.json").is_file());
        // Sharded artifacts-* are gone.
        assert!(!dist.join("artifacts-ubuntu-latest.json").exists());
        assert!(!dist.join("artifacts-macos-latest.json").exists());
        assert!(!dist.join("artifacts-windows-x86_64.json").exists());
        // Context shards SURVIVE — there's no un-suffixed replacement, so
        // we must not delete the only manifest the next retry could use.
        assert!(dist.join("context-ubuntu-latest.json").is_file());
        assert!(dist.join("context-macos-latest.json").is_file());
    }

    /// Drop must purge `Binary` + `UniversalBinary` (raw build outputs
    /// not in preserved-dist) while leaving every other kind in place —
    /// they survive publish unchanged.
    #[test]
    fn drop_stale_binary_artifacts_drops_binary_and_universal_binary_only() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::Verbosity;

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());

        // Seed: one of each kind we expect to drop + one of each kind
        // we expect to keep.
        let kinds_to_drop = [ArtifactKind::Binary, ArtifactKind::UniversalBinary];
        let kinds_to_keep = [
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Metadata,
        ];
        for (i, k) in kinds_to_drop.iter().chain(kinds_to_keep.iter()).enumerate() {
            ctx.artifacts.add(Artifact {
                kind: *k,
                name: format!("art-{i}"),
                path: std::path::PathBuf::from(format!("art-{i}")),
                target: None,
                crate_name: String::new(),
                metadata: Default::default(),
                size: None,
            });
        }

        let log = StageLogger::new("test", Verbosity::Quiet);
        drop_stale_binary_artifacts(&mut ctx, &log);

        // Both Binary kinds dropped.
        assert!(
            !ctx.artifacts
                .all()
                .iter()
                .any(|a| matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary)),
            "Binary and UniversalBinary must be dropped"
        );
        // All keep-kinds survive.
        for k in &kinds_to_keep {
            assert!(
                ctx.artifacts.all().iter().any(|a| a.kind == *k),
                "kind {:?} should have been kept",
                k
            );
        }
    }
}
