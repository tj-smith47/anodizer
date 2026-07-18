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
//! release / blob / publish / snapcraft-publish chain.
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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anodizer_core::artifact::ArtifactRegistry;
use anodizer_core::config::{Config, WorkspaceConfig};
use anodizer_core::context::Context;
use anodizer_core::git::short_commit_str;
use anodizer_core::log::StageLogger;

/// Layout of the preserved dist tree discovered at the dist root.
#[derive(Debug)]
pub(super) enum DistLayout {
    /// A flat `context.json` (and optional `context-<shard>.json`) at
    /// the dist root — today's single-crate layout.
    Flat,
    /// Per-crate subdirectories each containing a `context.json`. The
    /// `Vec<String>` carries the subdir names (= crate names) in
    /// filesystem order; callers topo-sort before iterating.
    PerCrate(Vec<String>),
    /// Both a flat `context.json` AND at least one per-crate subdir with
    /// a `context.json` exist — ambiguous; user must clean up.
    Ambiguous { crate_subdirs: Vec<String> },
}

/// Scan `dist/` to determine whether it uses the flat single-crate
/// layout, the per-crate subdir layout, or an ambiguous mix of both.
///
/// A "per-crate subdir" is any immediate subdirectory of `dist/` that
/// contains a `context.json` or `context-<shard>.json` file.
/// The flat layout is detected by `dist/context.json` or
/// `dist/context-*.json` at the root itself.
pub(super) fn detect_dist_layout(dist: &Path, log: &StageLogger) -> Result<DistLayout> {
    let has_flat = !discover_sharded_manifests(dist, anodizer_core::dist::CONTEXT_JSON)?.is_empty();

    let mut crate_subdirs: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: reading dist directory {} to detect layout",
            dist.display()
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            Err(e) => {
                // A `file_type()` failure (transient IO, dangling symlink)
                // routes the entry to "not a per-crate subdir", but surface
                // the reason so an operator debugging an unexpected Flat-vs-
                // PerCrate choice isn't left guessing why an entry was skipped.
                log.verbose(&format!(
                    "stat of dist entry {} failed: {e}; treating as non-directory",
                    entry.path().display()
                ));
                false
            }
        };
        if !is_dir {
            continue;
        }
        let subdir = entry.path();
        // A subdir counts as a per-crate preserve if it contains context.json
        // or context-<shard>.json.
        if !discover_sharded_manifests(&subdir, anodizer_core::dist::CONTEXT_JSON)?.is_empty()
            && let Some(name) = entry.file_name().to_str()
        {
            crate_subdirs.push(name.to_string());
        }
    }
    crate_subdirs.sort();

    match (has_flat, crate_subdirs.is_empty()) {
        (_, true) => Ok(DistLayout::Flat),
        (false, false) => Ok(DistLayout::PerCrate(crate_subdirs)),
        (true, false) => Ok(DistLayout::Ambiguous { crate_subdirs }),
    }
}

/// Whether `dist/<crate>/` exists and holds a preserved context
/// manifest (`context.json` or `context-<shard>.json`). Matches the
/// per-crate-subdir criterion used by [`detect_dist_layout`] so the
/// `--crate` dispatch in `mod.rs` routes to the same subdir layout the
/// no-flag auto-iteration would.
pub(super) fn crate_subdir_has_manifest(dist: &Path, crate_name: &str, log: &StageLogger) -> bool {
    let subdir = dist.join(crate_name);
    if !subdir.is_dir() {
        return false;
    }
    match discover_sharded_manifests(&subdir, anodizer_core::dist::CONTEXT_JSON) {
        Ok(manifests) => !manifests.is_empty(),
        Err(e) => {
            // A real read error (permissions, transient IO) routes the
            // crate to the flat path. Surface the reason so an operator
            // debugging an unexpected layout choice isn't left guessing
            // why a present subdir was skipped.
            log.verbose(&format!(
                "failed to scan {} for context manifests: {e}; \
                 treating crate '{crate_name}' as having no per-crate subdir",
                subdir.display()
            ));
            false
        }
    }
}

/// Knobs the dispatcher hands to `publish_only::run`. Credential and
/// signing-key presence is validated by the config-derived environment
/// preflight in `commands/release/mod.rs` (the github-release publisher's
/// token ladder + the sign stage's `KeyEnv` requirements), so nothing
/// inside this module re-checks credentials.
pub(super) struct RunOpts {
    pub dry_run: bool,
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
    run_one_crate_dist(ctx, config, log, &opts, dist)
}

/// Iterate per-crate subdirs in topo order, running the publish-only pipeline
/// once per crate. The artifact registry is reset between crates so each
/// pipeline sees only that crate's preserved artifacts. (Credentials were
/// already gated upstream by the config-derived environment preflight.)
///
/// `crate_order` is already topo-sorted by the caller (see `mod.rs`).
pub(super) fn run_per_crate(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    opts: RunOpts,
    dist_base: PathBuf,
    crate_order: Vec<String>,
) -> Result<()> {
    log.status(&format!(
        "iterating {} crate(s) in per-crate publish-only mode — {}",
        crate_order.len(),
        crate_order.join(", ")
    ));

    // Build a name → WorkspaceConfig index up-front so each iteration
    // can apply the right overlay in O(1). Workspace-based configs leave
    // top-level `config.crates` empty; per-crate iteration must scope
    // `ctx.config.crates` to the workspace containing the current
    // crate or stages like changelog see no crates and bail.
    let workspace_for: HashMap<String, &WorkspaceConfig> = config
        .workspaces
        .as_deref()
        .map(|ws_list| {
            let mut idx = HashMap::new();
            for ws in ws_list {
                for c in &ws.crates {
                    idx.insert(c.name.clone(), ws);
                }
            }
            idx
        })
        .unwrap_or_default();

    // Snapshot-and-restore the three fields the per-iteration overlay
    // mutates. Wrapped in an RAII guard (`PerCrateOverlayGuard`) so a
    // panic from `run_one_crate_dist` or any overlay/skip-merge call
    // still rolls the caller's `ctx` back to its pre-loop shape —
    // without it, an unwind would leak the last iteration's
    // `selected_crates` / `skip_stages` / `dist` overrides into any
    // outer `catch_unwind` boundary, leaving the caller's state
    // pointed at half-applied mid-iteration values.
    //
    // Why per-iteration scoping in the first place:
    //
    // * `selected_crates`: publishers resolve their effective crate
    //   set via `effective_publish_crates`, which falls back to the
    //   full crate universe (workspace-flattened) when this Vec is
    //   empty. Without scoping every publisher in cfgd-core's
    //   iteration would iterate every workspace crate, find no
    //   applicable config, and either skip-all (which the homebrew
    //   publisher classifies as a `failed` outcome to surface
    //   "nothing pushed") or attempt to publish for crates that
    //   aren't in the current iteration's preserved dist.
    // * `skip_stages`: regular release routes through
    //   `compute_skip_stages`, which merges `workspaces[].skip:`.
    //   Publish-only never went through that code, so a workspace
    //   declaring `skip: [announce]` (e.g. cfgd-core, a library
    //   crate that shouldn't broadcast an announcement) was
    //   silently ignored and announce ran anyway.
    // * `dist`: downstream metadata writers
    //   (`write_pre_release_metadata`, the GitHub uploader's
    //   relative-path resolver) read `ctx.config.dist` directly;
    //   without scoping every crate's metadata.json would land at
    //   the workspace-root `dist/` instead of its preserved subdir.
    let mut guard = PerCrateOverlayGuard::capture(ctx);
    // The saved baseline lives on the guard; copy it out once so the
    // per-iteration reset doesn't need a `&self` borrow that would
    // conflict with the `&mut ctx` reborrow further down.
    let baseline_skip_stages = guard.snapshot_skip_stages().to_vec();
    for crate_name in &crate_order {
        let crate_dist = dist_base.join(crate_name);
        log.status(&format!(
            "publishing crate '{crate_name}' from {}",
            crate_dist.display()
        ));
        // Rewind the config fields `apply_workspace_overlay` mutates
        // (changelog / signs / binary_signs / before / after / env /
        // crates / workspaces) to the pre-loop baseline before re-applying this
        // iteration's overlay. Without it a value set by a prior
        // workspace would leak into one that leaves it unset, and `env`
        // (appended, not replaced, by the overlay) would accumulate
        // every prior workspace's entries.
        guard.reset_overlay_fields();
        // Rewind the version-derived template vars to the pre-loop
        // baseline before re-anchoring this crate's version. Without it,
        // a crate whose preserved manifest records no version (where
        // `apply_per_crate_version` early-returns) would inherit the
        // prior crate's re-anchored version and render its tag / release
        // title / artifact names against the WRONG version.
        guard.reset_version_vars();
        // Rewind `ReleaseURL` so a URL the prior crate's release stage set
        // can't leak into a crate whose own release never derives one.
        guard.reset_release_url();
        let ctx = guard.ctx_mut();
        // Reset the artifact registry before each crate so artifacts
        // from a prior crate's pipeline don't leak into the next one's
        // sign/upload.
        ctx.artifacts = ArtifactRegistry::new();
        // Reset the prior iteration's publish outcome: each crate's
        // pipeline-end summary and required-failure gate must reflect
        // THIS crate only. A leftover report would render crate A's
        // publisher rows under crate B's Summary (and re-gate crate A's
        // failures), and a leftover publish_attempted would mislabel a
        // skipped publish as "aborted before dispatch".
        ctx.publish_report = None;
        ctx.publish_attempted = false;
        // Reset the prior iteration's verify-release verdict so each crate's
        // Summary reflects THIS crate's post-publish checks only. A leftover
        // Some would render crate A's verify findings under crate B's
        // Summary block.
        ctx.verify_release = None;
        // Reset skip_stages to the original baseline before re-applying
        // the workspace overlay so a skip from a prior iteration's
        // workspace doesn't leak forward.
        ctx.options.skip_stages = baseline_skip_stages.clone();
        if let Some(ws) = workspace_for.get(crate_name.as_str()) {
            crate::commands::helpers::apply_workspace_overlay(&mut ctx.config, ws);
            merge_workspace_skip(&mut ctx.options.skip_stages, &ws.skip);
        }
        // Re-anchor ctx.config.dist onto the per-crate preserved
        // subdir for the duration of this iteration.
        ctx.config.dist = crate_dist.clone();
        // Scope selected_crates to the current crate so every
        // publisher's effective-crates resolution sees a single entry
        // instead of the workspace-flattened fallback.
        ctx.options.selected_crates = vec![crate_name.clone()];
        // Re-anchor `Version` (and the vars derived from it) onto this
        // crate's preserved manifest BEFORE rendering its tag. The
        // upstream `resolve_git_context` set `Version` once from HEAD's
        // first-resolved crate; in a lockstep workspace every crate
        // shares that version, but in workspace per-crate
        // INDEPENDENT-version mode each crate carries its own version in
        // its preserved `context.json`. Without re-anchoring, a crate's
        // `tag_template` / release-title / artifact-name would render
        // against the wrong version and mint a mis-tagged GitHub release
        // (irreversible). Best-effort: a missing/unparseable preserved
        // version leaves the upstream `Version` in place.
        apply_per_crate_version(ctx, &crate_dist, crate_name, log);
        // Re-derive the per-crate `Tag` (and matching `PreviousTag`).
        // The upstream `resolve_git_context` set these once from the
        // first-resolved crate's `tag_template` against HEAD; every
        // iteration would otherwise inherit that single global tag,
        // titling each crate's GitHub release with the wrong tag and
        // skewing the changelog's current-tag / compare-link to a
        // foreign crate's tag. Rendering each crate's own `tag_template`
        // against the now-re-anchored per-crate `Version` recovers its
        // correct tag (`core-v0.4.0` for cfgd-core, `v0.4.0` for cfgd).
        apply_per_crate_tag(ctx, config, crate_name, log);
        let per_crate_opts = RunOpts {
            dry_run: opts.dry_run,
        };
        // Per-crate `before:` hooks fire at the START of this crate's scope —
        // after its version/tag template vars are anchored so the hooks render
        // against THIS crate's `Version` / `Tag`, and before its pipeline runs.
        run_per_crate_lifecycle_hooks(ctx, crate_name, HookKind::Before, opts.dry_run, log)?;
        run_one_crate_dist(ctx, config, log, &per_crate_opts, crate_dist)?;
        // Per-crate `after:` hooks fire at the END of this crate's scope, once
        // its publish dispatch has completed (still scoped to this crate's vars).
        run_per_crate_lifecycle_hooks(ctx, crate_name, HookKind::After, opts.dry_run, log)?;
    }
    // Explicit drop is redundant — `guard` falls out of scope and
    // restores at function exit — but it documents the restore point
    // for a reader.
    drop(guard);
    Ok(())
}

mod per_crate;
mod preserved;

use per_crate::*;
use preserved::*;

#[cfg(test)]
mod tests;
