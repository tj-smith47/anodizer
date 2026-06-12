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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anodizer_core::artifact::ArtifactRegistry;
use anodizer_core::config::{Config, WorkspaceConfig};
use anodizer_core::context::Context;
use anodizer_core::git::short_commit_str;
use anodizer_core::log::StageLogger;

use super::helpers;
use crate::pipeline;

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
    let has_flat = !discover_sharded_manifests(dist, "context")?.is_empty();

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
        if !discover_sharded_manifests(&subdir, "context")?.is_empty()
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
    match discover_sharded_manifests(&subdir, "context") {
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

/// Names of the env vars that gate the publish-only credential
/// preflight. Documented as a single source of truth so the error
/// message and the check itself stay in lockstep.
const SIGN_ENV_VARS: &[&str] = &["COSIGN_KEY", "GPG_PRIVATE_KEY"];
const GITHUB_TOKEN_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "ANODIZER_GITHUB_TOKEN"];

/// Knobs the dispatcher hands to `publish_only::run`. The credential
/// preflight is NOT among them: [`credential_preflight_gate`] runs in
/// the dispatcher (`commands/release/mod.rs`) before the failure-policy
/// boundary, so nothing inside this module re-checks credentials.
pub(super) struct RunOpts {
    pub dry_run: bool,
}

/// Publish-only credential gate, invoked by the dispatcher in
/// `commands/release/mod.rs` alongside the env and publisher-state
/// preflights — i.e. BEFORE the `release.on_failure` policy boundary.
/// A missing token or signing key aborts with zero mutations and must
/// surface as fix-and-re-run; routing it through the policy would let
/// a plain env mistake trigger a destructive rollback of a tag the
/// run never touched.
///
/// `--dry-run` skips the check so operators can preview the pipeline
/// without secrets; `--no-preflight` is the explicit opt-out for the
/// rare case where the operator wants the mid-pipeline failure instead.
pub(super) fn credential_preflight_gate(
    ctx: &Context,
    dry_run: bool,
    no_preflight: bool,
    log: &StageLogger,
) -> Result<()> {
    if dry_run {
        log.verbose("(dry-run) skipping production-credential preflight");
        return Ok(());
    }
    if no_preflight {
        log.warn(
            "credential preflight skipped via --no-preflight; \
             missing credentials will fail mid-pipeline (no idempotent recovery)",
        );
        return Ok(());
    }
    preflight_credentials(|k| ctx.env_var(k))
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
/// already gated upstream by [`credential_preflight_gate`].)
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
    //   set via `effective_publish_crates`, which falls back to
    //   `util::all_crates` (workspace-flattened) when this Vec is
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
        // crates) to the pre-loop baseline before re-applying this
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
        run_one_crate_dist(ctx, config, log, &per_crate_opts, crate_dist)?;
    }
    // Explicit drop is redundant — `guard` falls out of scope and
    // restores at function exit — but it documents the restore point
    // for a reader.
    drop(guard);
    Ok(())
}

/// RAII guard that snapshots the `ctx` fields mutated by
/// `run_per_crate`'s overlay loop (`config.dist`,
/// `options.selected_crates`, `options.skip_stages`, the per-crate
/// `Tag` / `PreviousTag` template vars, and the version-derived
/// template vars in [`VERSION_TEMPLATE_VARS`]) and restores them in
/// `Drop` so an unwind through the loop body still leaves the caller's
/// `ctx` pointed at its pre-loop shape.
///
/// The save/restore-via-closure pattern this replaces was
/// panic-unsafe: a panic from inside the iteration would skip the
/// post-closure restore lines, leaking mid-iteration override values
/// into any outer `catch_unwind` boundary (test harnesses, embedding
/// crates).
struct PerCrateOverlayGuard<'a> {
    ctx: &'a mut Context,
    saved_dist: std::path::PathBuf,
    saved_selected_crates: Vec<String>,
    saved_skip_stages: Vec<String>,
    saved_tag: Option<String>,
    saved_previous_tag: Option<String>,
    saved_release_url: Option<String>,
    saved_version_vars: Vec<(&'static str, Option<String>)>,
    saved_overlay: OverlayFields,
}

/// Template vars derived from the resolved `Version`. `run_per_crate`
/// re-anchors all of them onto each crate's preserved version (see
/// [`apply_per_crate_version`]); the guard snapshots and restores the
/// set so a per-iteration override can't leak past the loop.
const VERSION_TEMPLATE_VARS: &[&str] = &[
    "Version",
    "RawVersion",
    "Base",
    "Major",
    "Minor",
    "Patch",
    "Prerelease",
    "BuildMetadata",
];

/// Snapshot of the `config` fields `apply_workspace_overlay` mutates.
///
/// `apply_workspace_overlay` is conditional: it overwrites `changelog` /
/// `signs` / `binary_signs` / `before` / `after` only when the workspace
/// sets them, and *appends* to `env`. Without a per-iteration reset to
/// this baseline, a value set by workspace N would leak into workspace
/// N+1 (which left it unset), and `env` would accumulate every prior
/// workspace's entries. Capturing the baseline once lets each iteration
/// rewind these fields before applying its own overlay.
#[derive(Clone)]
struct OverlayFields {
    crates: Vec<anodizer_core::config::CrateConfig>,
    changelog: Option<anodizer_core::config::ChangelogConfig>,
    signs: Vec<anodizer_core::config::SignConfig>,
    binary_signs: Vec<anodizer_core::config::SignConfig>,
    before: Option<anodizer_core::config::HooksConfig>,
    after: Option<anodizer_core::config::HooksConfig>,
    env: Option<Vec<String>>,
}

impl OverlayFields {
    fn capture(config: &Config) -> Self {
        Self {
            crates: config.crates.clone(),
            changelog: config.changelog.clone(),
            signs: config.signs.clone(),
            binary_signs: config.binary_signs.clone(),
            before: config.before.clone(),
            after: config.after.clone(),
            env: config.env.clone(),
        }
    }

    fn restore_into(&self, config: &mut Config) {
        config.crates = self.crates.clone();
        config.changelog = self.changelog.clone();
        config.signs = self.signs.clone();
        config.binary_signs = self.binary_signs.clone();
        config.before = self.before.clone();
        config.after = self.after.clone();
        config.env = self.env.clone();
    }
}

impl<'a> PerCrateOverlayGuard<'a> {
    fn capture(ctx: &'a mut Context) -> Self {
        let saved_dist = ctx.config.dist.clone();
        let saved_selected_crates = ctx.options.selected_crates.clone();
        let saved_skip_stages = ctx.options.skip_stages.clone();
        let saved_tag = ctx.template_vars().get("Tag").cloned();
        let saved_previous_tag = ctx.template_vars().get("PreviousTag").cloned();
        let saved_release_url = ctx.template_vars().get("ReleaseURL").cloned();
        let saved_version_vars = VERSION_TEMPLATE_VARS
            .iter()
            .map(|&k| (k, ctx.template_vars().get(k).cloned()))
            .collect();
        let saved_overlay = OverlayFields::capture(&ctx.config);
        Self {
            ctx,
            saved_dist,
            saved_selected_crates,
            saved_skip_stages,
            saved_tag,
            saved_previous_tag,
            saved_release_url,
            saved_version_vars,
            saved_overlay,
        }
    }

    /// Rewind the overlay-mutated `config` fields to the captured
    /// baseline. Call at the start of each iteration *before*
    /// `apply_workspace_overlay` so a value set (or appended) by a prior
    /// workspace can't leak into the current one.
    fn reset_overlay_fields(&mut self) {
        let saved = self.saved_overlay.clone();
        saved.restore_into(&mut self.ctx.config);
    }

    /// Per-iteration `skip_stages` reset baseline. Returns the snapshot
    /// the guard took at capture-time so the loop can rewind to the
    /// pre-overlay value before applying the current workspace's
    /// `skip:` list.
    fn snapshot_skip_stages(&self) -> &[String] {
        &self.saved_skip_stages
    }

    /// Rewind the version-derived template vars to the captured baseline.
    /// Call at the start of each iteration *before* `apply_per_crate_version`
    /// so a version re-anchored for a prior crate can't leak into one whose
    /// preserved manifest records no version (where `apply_per_crate_version`
    /// early-returns and leaves the vars untouched). Mirrors the
    /// `baseline_skip_stages` reset; the Drop-restore at loop end still
    /// returns the caller's pre-loop values.
    fn reset_version_vars(&mut self) {
        for (key, value) in &self.saved_version_vars {
            match value {
                Some(v) => self.ctx.template_vars_mut().set(key, v),
                None => {
                    self.ctx.template_vars_mut().unset(key);
                }
            }
        }
    }

    /// Rewind `ReleaseURL` to the captured baseline. Call at the start of
    /// each iteration: the release stage sets the var per crate, but only
    /// on paths that resolve a repo — a crate whose release is skipped or
    /// has no resolvable repo would otherwise inherit the PRIOR crate's
    /// URL, and its metadata.json / announce templates would point at a
    /// foreign crate's release.
    fn reset_release_url(&mut self) {
        match &self.saved_release_url {
            Some(v) => self.ctx.template_vars_mut().set("ReleaseURL", v),
            None => {
                self.ctx.template_vars_mut().unset("ReleaseURL");
            }
        }
    }

    /// Reborrow the wrapped `&mut Context` for one loop iteration.
    /// Bypasses the borrow that would otherwise pin the original `ctx`
    /// alias for the entire lifetime of the guard.
    fn ctx_mut(&mut self) -> &mut Context {
        self.ctx
    }
}

impl Drop for PerCrateOverlayGuard<'_> {
    /// Restore the pre-overlay `ctx` state by *moving* each captured
    /// value back into the context via `std::mem::take`. After the
    /// move the guard's own fields hold defaulted (empty / zero-sized)
    /// stand-ins — that's intentional: a hypothetical second drop would
    /// only re-assign those defaults to the context, not corrupt it
    /// with stale data. But the inverse — wrapping the guard in
    /// `ManuallyDrop` or `mem::forget`ing it — would skip the restore
    /// entirely and leak the per-iteration overlay into the next
    /// iteration's `ctx`, so neither is supported. The standard RAII
    /// drop path is the only sound consumption.
    fn drop(&mut self) {
        let saved_overlay = self.saved_overlay.clone();
        saved_overlay.restore_into(&mut self.ctx.config);
        self.ctx.config.dist = std::mem::take(&mut self.saved_dist);
        self.ctx.options.selected_crates = std::mem::take(&mut self.saved_selected_crates);
        self.ctx.options.skip_stages = std::mem::take(&mut self.saved_skip_stages);
        match self.saved_tag.take() {
            Some(tag) => self.ctx.template_vars_mut().set("Tag", &tag),
            None => {
                self.ctx.template_vars_mut().unset("Tag");
            }
        }
        match self.saved_previous_tag.take() {
            Some(prev) => self.ctx.template_vars_mut().set("PreviousTag", &prev),
            None => {
                self.ctx.template_vars_mut().unset("PreviousTag");
            }
        }
        match self.saved_release_url.take() {
            Some(url) => self.ctx.template_vars_mut().set("ReleaseURL", &url),
            None => {
                self.ctx.template_vars_mut().unset("ReleaseURL");
            }
        }
        for (key, value) in std::mem::take(&mut self.saved_version_vars) {
            match value {
                Some(v) => self.ctx.template_vars_mut().set(key, &v),
                None => {
                    self.ctx.template_vars_mut().unset(key);
                }
            }
        }
    }
}

/// Set `ctx`'s `Tag` (and a prefix-matched `PreviousTag`) for the crate
/// currently being published.
///
/// Locates the crate's `tag_template` (in `ctx.config.crates` after the
/// workspace overlay, falling back to the workspace list on `config`),
/// renders it against the already-resolved `Version`, and writes the
/// result to the `Tag` template var. `PreviousTag` is re-derived with
/// the crate's tag prefix so the changelog compare-link resolves
/// `<crate-prev>...<crate-tag>` rather than spanning a foreign crate's
/// tag. Best-effort: a missing `tag_template` or a git lookup failure
/// leaves the upstream value in place rather than aborting the publish.
///
/// Rendering `tag_template` (rather than reusing a preserved per-crate
/// `git_tag`) is the only option here: publish-only rehydrates from the
/// `PreservedDistContext` manifest written by `check determinism
/// --preserve-dist`, whose schema carries only `artifacts` / `targets` /
/// `version` / `commit` — no tag field. The `git_tag` persisted by
/// `release --split` lives in a different (`SplitContext`) manifest that
/// this path never deserializes, so there is no preserved tag to prefer.
/// Re-rendering against the shared, already-resolved `Version` recovers
/// each crate's correct tag.
/// Re-anchor `ctx`'s `Version` (and the vars derived from it —
/// `RawVersion`, `Base`, `Major`, `Minor`, `Patch`, `Prerelease`,
/// `BuildMetadata`) onto the version recorded in the crate's preserved
/// `context.json`.
///
/// In a lockstep workspace every crate shares the single `Version` that
/// `resolve_git_context` resolved from HEAD, so this is a no-op rewrite
/// of the same value. In workspace per-crate INDEPENDENT-version mode
/// each crate's preserved manifest carries its own version; without this
/// re-anchor the crate's `tag_template`, release title, and artifact
/// names would all render against the first-resolved crate's version and
/// mint a mis-tagged GitHub release.
///
/// Best-effort: a missing preserved manifest or a non-semver version
/// string leaves the upstream `Version` vars in place rather than
/// aborting the publish.
fn apply_per_crate_version(
    ctx: &mut Context,
    crate_dist: &Path,
    crate_name: &str,
    log: &StageLogger,
) {
    let Some(version) = peek_preserved_version(crate_dist) else {
        return;
    };
    let semver = match anodizer_core::git::parse_semver(&version) {
        Ok(sv) => sv,
        Err(e) => {
            log.verbose(&format!(
                "preserved version '{version}' for crate '{crate_name}' \
                 is not strict semver ({e}); leaving Version vars unchanged"
            ));
            return;
        }
    };

    let vars = ctx.template_vars_mut();
    vars.set("Version", &semver.version_string());
    vars.set("RawVersion", &semver.raw_version_string());
    vars.set("Base", &semver.raw_version_string());
    vars.set("Major", &semver.major.to_string());
    vars.set("Minor", &semver.minor.to_string());
    vars.set("Patch", &semver.patch.to_string());
    vars.set("Prerelease", semver.prerelease.as_deref().unwrap_or(""));
    vars.set(
        "BuildMetadata",
        semver.build_metadata.as_deref().unwrap_or(""),
    );
}

/// Read the canonical version recorded in a crate's preserved dist
/// (`context.json` / `context-<shard>.json`). Returns the first
/// non-empty version found; `None` when no manifest exists, none records
/// a version, or discovery fails. The full cross-shard version/commit
/// consistency check runs later in `run_one_crate_dist` — this peek only
/// needs a value to re-anchor the per-crate template vars before tag
/// rendering.
fn peek_preserved_version(crate_dist: &Path) -> Option<String> {
    let contexts = discover_preserved_contexts(crate_dist).ok()?;
    contexts
        .into_iter()
        .map(|(_, c)| c.version)
        .find(|v| !v.is_empty())
}

fn apply_per_crate_tag(ctx: &mut Context, config: &Config, crate_name: &str, log: &StageLogger) {
    let tag_template = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .or_else(|| {
            config
                .workspaces
                .as_deref()
                .into_iter()
                .flatten()
                .flat_map(|ws| ws.crates.iter())
                .find(|c| c.name == crate_name)
        })
        .map(|c| c.tag_template.clone());
    let Some(tag_template) = tag_template.filter(|t| !t.is_empty()) else {
        return;
    };

    let tag = match ctx.render_template(&tag_template) {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return,
        Err(e) => {
            log.warn(&format!(
                "failed to render tag_template '{tag_template}' for crate '{crate_name}': {e}"
            ));
            return;
        }
    };
    ctx.template_vars_mut().set("Tag", &tag);

    let crate_prefix = anodizer_core::git::extract_tag_prefix(&tag_template);
    let prefix = crate_prefix
        .as_deref()
        .or_else(|| config.monorepo_tag_prefix());
    match anodizer_core::git::find_previous_tag_with_prefix(
        &tag,
        config.git.as_ref(),
        Some(ctx.template_vars()),
        prefix,
    ) {
        Ok(Some(prev)) => ctx.template_vars_mut().set("PreviousTag", &prev),
        Ok(None) => {
            ctx.template_vars_mut().unset("PreviousTag");
        }
        Err(e) => log.verbose(&format!(
            "previous-tag lookup for crate '{crate_name}' failed: {e}"
        )),
    }
}

/// Merge a workspace's `skip:` list into the iteration's effective
/// `skip_stages`, deduping so an already-present stage (set by CLI or a
/// prior iteration's restore) doesn't appear twice.
///
/// Extracted from the per-crate loop so the dedup contract is unit-
/// testable without standing up a full Context/Config fixture.
fn merge_workspace_skip(into: &mut Vec<String>, ws_skip: &[String]) {
    for stage in ws_skip {
        if !into.iter().any(|s| s == stage) {
            into.push(stage.clone());
        }
    }
}

/// Inner body of the publish-only pipeline for a single dist root.
/// Called by both `run()` (flat layout) and `run_per_crate()` (per-crate layout).
fn run_one_crate_dist(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    opts: &RunOpts,
    dist: PathBuf,
) -> Result<()> {
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
        "loaded {} context manifest(s) (version={}, commit={}, targets=[{}], {} artifact(s))",
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
    // shard's manifest contributes its artifacts to the registry.
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

    // Cross-shard cross-target artifacts (source archive, install.sh,
    // metadata.json — all `target: None`) appear in every shard's
    // manifest by design. Each shard's harness runs them identically;
    // download-artifact merge-multiple collapses the on-disk copies to
    // one. Drop the redundant registry entries here so SignStage /
    // ReleaseStage don't try to re-sign or re-upload the same path
    // multiple times. Per-target duplicates (matrix overlap bugs) are
    // preserved so `detect_duplicate_artifact_paths` below still
    // catches them.
    ctx.artifacts.dedupe_targetless_duplicates();

    log.status(&format!(
        "rehydrated {} artifact(s) from {} artifacts manifest(s)",
        ctx.artifacts.all().len(),
        artifact_manifests.len(),
    ));

    // Fail closed on duplicate artifact paths across the merged
    // manifests. After dedup of cross-shard cross-target duplicates
    // (source.tar.gz, install.sh, metadata.json — target: None,
    // produced identically on every shard), any remaining same-path
    // entry must come from a per-target overlap: two shards both
    // claimed they built for the same target. That's a matrix bug or
    // hand-edited manifest; re-signing duplicate entries would
    // produce double-emit confusion in SignStage / ReleaseStage.
    detect_duplicate_artifact_paths(ctx)?;

    // ── Strip ephemeral signatures / certificates ──────────────────────
    // Defensive: the harness skips SignStage when production keys are
    // exported on the runner, so preserved-dist usually has no `.sig`
    // / `.asc` files. But re-signing on top of an existing chain (e.g.
    // operator ran the harness without prod keys, then brought them
    // in) would emit `*.sig.sig` / `*.pem.sig` — corrupt checksums
    // and confuse downstream verifiers. Strip up-front so `SignStage`
    // always sees a clean input registry.
    //
    // Runs BEFORE `detect_missing_files`: any Signature / Certificate
    // entry that lives under `.det-tmp/target/.../<bin>.sig` is a
    // per-binary signature the harness produced when `binary_signs` was
    // configured. `upload-artifact@v4` excludes hidden directories
    // (`.det-tmp/`) by default, so those paths never reach the publish
    // job's disk. Stripping the registry entries here makes them invisible
    // to the existence check below — they'd otherwise trip a false
    // "preserved dist is incomplete" bail. SignStage doesn't re-create
    // binary signatures in publish-only mode (binary_signs is cleared
    // above), which matches the action's hidden-files-excluded reality.
    strip_ephemeral_signatures(ctx, log);

    // Filesystem vs manifest cross-check: every artifact path the
    // manifest references must actually exist on disk. Missing files
    // means the preserved dist is incomplete — running through to
    // SignStage would fail with a less actionable error from
    // cosign/gpg, so we surface it here with a manifest-shaped
    // diagnostic instead. We do NOT flag unreferenced files (the
    // dist tree carries metadata.json, harness logs, etc. that aren't
    // in the artifacts manifest).
    //
    // Skipped artifact kinds:
    //   * Binary + UniversalBinary — paths under `.det-tmp/target/...`
    //     are intermediate raw cargo output, never preserved. Publishers
    //     that consume Binary artifacts (nix's DynamicallyLinked,
    //     winget's binary filename) read ONLY metadata, not the file
    //     itself, so the path mismatch is harmless.
    //   * Metadata — `dist/metadata.json` is renamed per-shard by the
    //     action's preserve step (`metadata-<shard>.json`) before
    //     upload, so the canonical un-suffixed path NEVER exists on the
    //     publish job's disk pre-pipeline. `run_post_pipeline` rewrites
    //     the canonical file at the end of publish-only from the merged
    //     registry, so the existence check is trying to verify a file
    //     this pipeline itself will produce — a layering violation.
    crate::commands::helpers::detect_missing_files(
        ctx.artifacts
            .all()
            .iter()
            .filter(|a| {
                !matches!(
                    a.kind,
                    anodizer_core::artifact::ArtifactKind::Binary
                        | anodizer_core::artifact::ArtifactKind::UniversalBinary
                        | anodizer_core::artifact::ArtifactKind::Metadata
                )
            })
            .map(|a| a.path.as_path()),
        &dist,
    )?;

    // ── Materialize metadata.json for the release upload ───────────────
    // The release upload set includes the `Metadata` artifact (when
    // `include_meta` is on), whose path resolves to
    // `<dist>/metadata.json`. The preserve-dist flow rehydrates the
    // registry from per-crate manifests but never writes that file —
    // only `run_post_pipeline` does, and that runs *after* the upload.
    // Write it now (after the per-crate `Tag` is set, so it carries the
    // correct tag/version/commit) so ReleaseStage's existence check
    // doesn't bail before the draft→published promotion. The registry
    // entry already points here; no `add` needed.
    crate::commands::helpers::write_metadata_json(ctx, config, log)?;

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
        "stripping {count} ephemeral signature/certificate artifact(s) before re-sign"
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
                "failed to delete stale signature {}: {} \
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
        "stripped {count} ephemeral signature artifact(s) from registry \
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
const EPHEMERAL_SIGNATURE_SUFFIXES: &[&str] = &[".sig", ".asc", ".pem"];

fn is_ephemeral_signature_path(path: &str) -> bool {
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
fn hash_verify_preserved_dist(ctx: &PreservedDistContext, dist_root: &Path) -> Result<()> {
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
fn cleanup_shard_manifests(dist: &Path, log: &StageLogger) {
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

    /// Regression test for the multi-shard ephemeral-signature
    /// false-positive. cosign's ECDSA nonce makes per-shard signatures
    /// of identical content diverge by design; each shard's context.json
    /// records its own .sig hash, but only ONE shard's file wins the
    /// `actions/download-artifact merge-multiple: true` race. The merged
    /// context references the others' hashes which CANNOT match the
    /// surviving bytes. Since `strip_ephemeral_signatures` discards
    /// these files and `SignStage` produces the production-key
    /// signatures, the hash-verify must skip them rather than block
    /// the publish.
    #[test]
    fn hash_verify_preserved_dist_skips_ephemeral_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        // Plant a `.sig` whose bytes do NOT match the recorded hash.
        // A non-skipping verify would error here.
        std::fs::write(tmp.path().join("foo.tar.gz.sha256.sig"), b"shard-A-bytes").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![PreservedArtifact {
                name: "foo.tar.gz.sha256.sig".into(),
                path: "foo.tar.gz.sha256.sig".into(),
                // Hash of unrelated bytes — exercises the skip path.
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 13,
            }],
            ..PreservedDistContext::default()
        };
        hash_verify_preserved_dist(&ctx, tmp.path())
            .expect("ephemeral .sig paths must skip hash-verify");
    }

    #[test]
    fn hash_verify_preserved_dist_skips_pem_and_asc() {
        // Same guarantee for the `.pem` (cosign cert) and `.asc` (gpg
        // armored sig) suffixes. Both are produced by SignStage's
        // ephemeral path and replaced on re-sign.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.pem"), b"cert-A").unwrap();
        std::fs::write(tmp.path().join("foo.asc"), b"asc-A").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![
                PreservedArtifact {
                    name: "foo.pem".into(),
                    path: "foo.pem".into(),
                    sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                    size: 6,
                },
                PreservedArtifact {
                    name: "foo.asc".into(),
                    path: "foo.asc".into(),
                    sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                    size: 5,
                },
            ],
            ..PreservedDistContext::default()
        };
        hash_verify_preserved_dist(&ctx, tmp.path())
            .expect("ephemeral .pem / .asc paths must skip hash-verify");
    }

    /// Regression: cross-shard duplicate paths with diverging recorded
    /// hashes (e.g. `anodizer-<ver>-source.tar.gz` produced
    /// independently on every shard with subtle git/tar/locale variance)
    /// land in the merged context multiple times. Only ONE shard's bytes
    /// survive `download-artifact merge-multiple` on disk; the others'
    /// claims cannot match. hash-verify must accept the path as soon as
    /// the disk bytes match ANY shard's recorded hash, not bail because
    /// some shards disagree with disk.
    #[test]
    fn hash_verify_preserved_dist_accepts_when_any_shard_matches_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("source.tar.gz"), b"hello world").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![
                // Shard A: WRONG hash (would fail alone).
                PreservedArtifact {
                    name: "source.tar.gz".into(),
                    path: "source.tar.gz".into(),
                    sha256:
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .into(),
                    size: 11,
                },
                // Shard B: correct hash → verifies the merged context.
                PreservedArtifact {
                    name: "source.tar.gz".into(),
                    path: "source.tar.gz".into(),
                    sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                    size: 11,
                },
                // Shard C: another WRONG hash (asserts iteration doesn't
                // short-circuit on the first mismatch).
                PreservedArtifact {
                    name: "source.tar.gz".into(),
                    path: "source.tar.gz".into(),
                    sha256:
                        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                            .into(),
                    size: 11,
                },
            ],
            ..PreservedDistContext::default()
        };
        hash_verify_preserved_dist(&ctx, tmp.path())
            .expect("cross-shard duplicate must verify when any shard's hash matches disk");
    }

    /// Counterpart: if NO shard's recorded hash matches disk, the
    /// verifier must still bail and surface every shard's expected hash
    /// in the error so the operator can audit which shards diverged.
    #[test]
    fn hash_verify_preserved_dist_bails_when_no_shard_matches_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("source.tar.gz"), b"hello world").unwrap();
        let ctx = PreservedDistContext {
            artifacts: vec![
                PreservedArtifact {
                    name: "source.tar.gz".into(),
                    path: "source.tar.gz".into(),
                    sha256:
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .into(),
                    size: 11,
                },
                PreservedArtifact {
                    name: "source.tar.gz".into(),
                    path: "source.tar.gz".into(),
                    sha256:
                        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                            .into(),
                    size: 11,
                },
            ],
            ..PreservedDistContext::default()
        };
        let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("recorded across 2 shard(s)"),
            "error must surface the shard count; got: {msg}"
        );
        assert!(
            msg.contains("source.tar.gz"),
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
            msg.contains("hashing preserved artifact"),
            "error must surface the hash-failure wording; got: {msg}"
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

    /// Filter contract for the inlined missing-file check: Binary +
    /// UniversalBinary kinds must be skipped (their paths live under
    /// `.det-tmp/target/...` and are not preserved into `dist/`),
    /// while every other kind flows through to
    /// `detect_missing_files`. Pin the filter shape so a refactor
    /// can't silently re-include Binary kinds and break the
    /// determinism-verified → publish flow.
    #[test]
    fn missing_file_check_skips_binary_and_universal_binary_kinds() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());

        // Seed Binary + UniversalBinary (should be filtered out) and
        // a couple of other kinds (should flow through).
        let kinds = [
            ArtifactKind::Binary,
            ArtifactKind::UniversalBinary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
        ];
        for (i, k) in kinds.iter().enumerate() {
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

        // Apply the same filter the run() call site uses and verify
        // exactly the non-Binary kinds survive.
        let kept: Vec<ArtifactKind> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| !matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary))
            .map(|a| a.kind)
            .collect();

        assert_eq!(kept, vec![ArtifactKind::Archive, ArtifactKind::Checksum]);
    }

    // ── detect_dist_layout tests ──────────────────────────────────────────────

    fn write_context_file(dir: &std::path::Path, name: &str) {
        let content = r#"{"artifacts":[],"targets":[],"version":"0.0.0","commit":"abc"}"#;
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn layout_test_log() -> StageLogger {
        StageLogger::new("layout-test", anodizer_core::log::Verbosity::Quiet)
    }

    #[test]
    fn detect_layout_flat_single_context() {
        let tmp = tempfile::tempdir().unwrap();
        write_context_file(tmp.path(), "context.json");
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        assert!(
            matches!(layout, super::DistLayout::Flat),
            "expected Flat, got {layout:?}"
        );
    }

    #[test]
    fn detect_layout_flat_sharded_context() {
        let tmp = tempfile::tempdir().unwrap();
        write_context_file(tmp.path(), "context-linux.json");
        write_context_file(tmp.path(), "context-macos.json");
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        assert!(
            matches!(layout, super::DistLayout::Flat),
            "expected Flat, got {layout:?}"
        );
    }

    #[test]
    fn detect_layout_per_crate_two_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("core");
        let b = tmp.path().join("cli");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        write_context_file(&a, "context.json");
        write_context_file(&b, "context.json");
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        match layout {
            super::DistLayout::PerCrate(names) => {
                let mut sorted = names.clone();
                sorted.sort();
                assert_eq!(sorted, vec!["cli", "core"]);
            }
            other => panic!("expected PerCrate, got {other:?}"),
        }
    }

    #[test]
    fn detect_layout_ambiguous_flat_and_per_crate() {
        let tmp = tempfile::tempdir().unwrap();
        write_context_file(tmp.path(), "context.json");
        let sub = tmp.path().join("core");
        std::fs::create_dir_all(&sub).unwrap();
        write_context_file(&sub, "context.json");
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        assert!(
            matches!(layout, super::DistLayout::Ambiguous { .. }),
            "expected Ambiguous, got {layout:?}"
        );
    }

    #[test]
    fn detect_layout_empty_dist_returns_flat() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        assert!(
            matches!(layout, super::DistLayout::Flat),
            "empty dist must return Flat, got {layout:?}"
        );
    }

    #[test]
    fn detect_layout_subdir_without_context_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write_context_file(tmp.path(), "context-linux.json");
        let sub = tmp.path().join("random-dir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("artifact.tar.gz"), b"bytes").unwrap();
        let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
        assert!(
            matches!(layout, super::DistLayout::Flat),
            "subdir without context.json must not count as per-crate, got {layout:?}"
        );
    }

    // ── merge_workspace_skip ─────────────────────────────────────────

    #[test]
    fn merge_workspace_skip_appends_new_entries() {
        let mut into: Vec<String> = vec![];
        super::merge_workspace_skip(&mut into, &["announce".to_string(), "publish".to_string()]);
        assert_eq!(into, vec!["announce", "publish"]);
    }

    #[test]
    fn merge_workspace_skip_dedupes_existing_cli_entries() {
        // CLI-supplied `--skip announce` plus a workspace
        // `skip: [announce, blob]` must NOT yield `[announce, announce, blob]`
        // — the dedup keeps each stage exactly once so the
        // `should_skip` lookup short-circuits as soon as it finds the
        // first match.
        let mut into: Vec<String> = vec!["announce".to_string()];
        super::merge_workspace_skip(&mut into, &["announce".to_string(), "blob".to_string()]);
        assert_eq!(into, vec!["announce", "blob"]);
    }

    #[test]
    fn merge_workspace_skip_empty_ws_is_noop() {
        let mut into: Vec<String> = vec!["snapcraft-publish".to_string()];
        super::merge_workspace_skip(&mut into, &[]);
        assert_eq!(into, vec!["snapcraft-publish"]);
    }

    /// Regression: prior to the fix, publish-only per-crate iteration
    /// applied the workspace overlay but never propagated
    /// `workspaces[].skip:` into the iteration's effective skip list.
    /// cfgd-core (a library workspace declaring `skip: [announce]`)
    /// ran announce anyway and failed rendering templates that depend
    /// on stage-release outputs the announce stage never saw a release
    /// from. This asserts the dedup behavior that gates the propagation.
    #[test]
    fn merge_workspace_skip_propagates_cfgd_core_announce_skip() {
        let mut into: Vec<String> = vec![];
        // Mirrors cfgd's `workspaces[name=cfgd-core].skip: [announce]`.
        super::merge_workspace_skip(&mut into, &["announce".to_string()]);
        assert!(
            into.iter().any(|s| s == "announce"),
            "workspace-level announce skip must propagate; got {:?}",
            into
        );
    }

    // ── run_per_crate dist restore ───────────────────────────────────

    /// Regression: `run_per_crate` re-anchors `ctx.config.dist` onto
    /// the per-crate preserved subdir for the duration of each
    /// iteration so downstream code reading `ctx.config.dist`
    /// (`write_pre_release_metadata`, the GitHub uploader's
    /// relative-path resolver) sees the active crate's preserved
    /// location. The pre-fix code left `ctx.config.dist` pointing at
    /// the workspace-root `./dist`, so cfgd's per-crate metadata.json
    /// landed in the wrong place. The save/restore must hold even
    /// when the iteration body errors out — otherwise a partial
    /// publish-only run would leak the per-iteration dist into the
    /// caller's context.
    #[test]
    fn run_per_crate_restores_ctx_config_dist_on_error() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let original_dist = tmp.path().join("dist");
        config.dist = original_dist.clone();
        let mut ctx = Context::new(config.clone(), ContextOptions::default());

        // `dist_base` points at a path that doesn't exist; `run_per_crate`
        // will iterate to the first crate, then `run_one_crate_dist`
        // will fail at `detect_dist_layout` / preserved-context discovery.
        // The dist-restore logic must still fire on the Err branch.
        let dist_base = tmp.path().join("missing");
        let log = anodizer_core::log::StageLogger::new(
            "publish-only-restore-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let opts = RunOpts { dry_run: true };
        let result = run_per_crate(
            &mut ctx,
            &config,
            &log,
            opts,
            dist_base,
            vec!["cfgd".to_string()],
        );
        assert!(
            result.is_err(),
            "iteration must fail when dist_base is absent — fixture precondition"
        );
        assert_eq!(
            ctx.config.dist, original_dist,
            "ctx.config.dist must be restored after the iteration (Ok or Err) \
             so the per-iteration override never leaks into the caller's context"
        );
    }

    /// Seed `<dist_base>/<name>/` with a minimal but valid EMPTY
    /// preserved dist (zero artifacts, commit `deadbeef`) so a
    /// `run_per_crate` iteration over `name` runs the real publish-only
    /// pipeline to completion in dry-run mode.
    fn seed_valid_preserved_dist(dist_base: &std::path::Path, name: &str) {
        let crate_dist = dist_base.join(name);
        std::fs::create_dir_all(&crate_dist).unwrap();
        std::fs::write(
            crate_dist.join("context.json"),
            r#"{"artifacts":[],"targets":[],"version":"0.0.0","commit":"deadbeef"}"#,
        )
        .unwrap();
        std::fs::write(crate_dist.join("artifacts.json"), "[]").unwrap();
    }

    /// Build the dry-run `Context` matching [`seed_valid_preserved_dist`]'s
    /// commit/version so the preserved-context cross-checks pass.
    fn preserved_dist_ctx(
        config: &anodizer_core::config::Config,
    ) -> anodizer_core::context::Context {
        use anodizer_core::context::{Context, ContextOptions};
        let mut ctx = Context::new(
            config.clone(),
            ContextOptions {
                dry_run: true,
                ..ContextOptions::default()
            },
        );
        ctx.template_vars_mut().set("FullCommit", "deadbeef");
        ctx.template_vars_mut().set("Version", "0.0.0");
        ctx.template_vars_mut().set("Tag", "v0.0.0");
        ctx
    }

    /// Each per-crate iteration owns its publish outcome: a leftover
    /// `publish_report` / `publish_attempted` from a prior iteration (or
    /// an outer run) would render the wrong publisher rows under the
    /// next crate's Summary, re-gate the prior crate's failures, and
    /// mislabel a skipped publish as "aborted before dispatch". The loop
    /// must clear both at EVERY iteration top, not once before the loop.
    ///
    /// Two-crate fixture: crate 'a' carries a minimal but valid empty
    /// preserved dist, so iteration 1 runs the real publish-only
    /// pipeline (`PublishStage::run` marks `publish_attempted` before
    /// its guards). Crate 'b' has no subdir, so iteration 2 fails at
    /// preserved-context discovery — AFTER its loop-top reset. A reset
    /// hoisted above the loop would clear only the pre-seeded outer
    /// state and leave iteration 1's outcome behind, failing the final
    /// asserts — this pins the per-iteration placement, not just
    /// outer-stale clearing.
    #[test]
    fn run_per_crate_resets_publish_outcome_each_iteration() {
        use anodizer_core::config::Config;
        use anodizer_core::publish_report::PublishReport;

        let tmp = tempfile::tempdir().unwrap();
        let dist_base = tmp.path().join("dist");
        seed_valid_preserved_dist(&dist_base, "a");

        let config = Config {
            dist: dist_base.clone(),
            ..Config::default()
        };
        let mut ctx = preserved_dist_ctx(&config);
        // Pre-seed stale OUTER state as well: a loop-hoisted reset would
        // clear this much, so the distinguishing signal below stays
        // iteration 1's freshly-set outcome.
        ctx.set_publish_report(PublishReport::default());
        ctx.set_publish_attempted();

        let log = anodizer_core::log::StageLogger::new(
            "publish-only-reset-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let opts = RunOpts { dry_run: true };
        let err = run_per_crate(
            &mut ctx,
            &config,
            &log,
            opts,
            dist_base.clone(),
            vec!["a".to_string(), "b".to_string()],
        )
        .expect_err("iteration 2 must fail on the absent dist/b subdir");
        let chain = format!("{err:#}");
        assert!(
            chain.contains(&dist_base.join("b").display().to_string()),
            "iteration 1 must succeed and iteration 2 must be the failing one \
             (otherwise this test never observes the per-iteration reset); got: {chain}"
        );
        assert!(
            ctx.publish_report().is_none(),
            "iteration 1's publish_report must be cleared at iteration 2's top"
        );
        assert!(
            !ctx.publish_attempted(),
            "iteration 1's publish_attempted must be cleared at iteration 2's top"
        );
    }

    /// Vacuity guard for the reset test above: prove the fixture's
    /// single successful iteration really exercises the
    /// `set_publish_attempted` setter. If `PublishStage::run` ever stops
    /// marking the attempt unconditionally (today it fires right after
    /// the snapshot guard), the reset test would degrade into asserting
    /// "still-cleared state stayed cleared" without noticing — this
    /// assert catches that drift loudly.
    #[test]
    fn run_per_crate_pipeline_marks_publish_attempted() {
        use anodizer_core::config::Config;

        let tmp = tempfile::tempdir().unwrap();
        let dist_base = tmp.path().join("dist");
        seed_valid_preserved_dist(&dist_base, "a");

        let config = Config {
            dist: dist_base.clone(),
            ..Config::default()
        };
        let mut ctx = preserved_dist_ctx(&config);
        let log = anodizer_core::log::StageLogger::new(
            "publish-only-vacuity-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let opts = RunOpts { dry_run: true };
        run_per_crate(
            &mut ctx,
            &config,
            &log,
            opts,
            dist_base,
            vec!["a".to_string()],
        )
        .expect("single valid-crate iteration must run the pipeline to completion");
        assert!(
            ctx.publish_attempted(),
            "the fixture's pipeline run must mark publish_attempted — \
             otherwise the reset test's distinguishing signal is gone"
        );
    }

    /// Build a crate config with a GitHub release block so a per-crate
    /// dry-run iteration drives the release stage's `ReleaseURL`
    /// derivation end-to-end.
    fn released_crate_cfg(name: &str, tag_template: &str) -> anodizer_core::config::CrateConfig {
        anodizer_core::config::CrateConfig {
            name: name.to_string(),
            tag_template: tag_template.to_string(),
            release: Some(anodizer_core::config::ReleaseConfig {
                github: Some(anodizer_core::config::ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Workspace per-crate mode, end-to-end through the real publish-only
    /// pipeline in dry-run: each crate's `dist/<crate>/metadata.json` must
    /// carry that crate's OWN release URL (derived from its own per-crate
    /// tag), not the prior iteration's. This is the file the action-side
    /// `release-url` output reads via `.release_url`.
    #[test]
    #[serial_test::serial]
    fn run_per_crate_metadata_carries_per_crate_release_url() {
        use anodizer_core::config::Config;

        let tmp = tempfile::tempdir().unwrap();
        let dist_base = tmp.path().join("dist");
        seed_valid_preserved_dist(&dist_base, "a");
        seed_valid_preserved_dist(&dist_base, "b");

        let config = Config {
            dist: dist_base.clone(),
            crates: vec![
                released_crate_cfg("a", "a-v{{ Version }}"),
                released_crate_cfg("b", "b-v{{ Version }}"),
            ],
            ..Config::default()
        };
        let mut ctx = preserved_dist_ctx(&config);
        // The changelog stage shells to git in the process cwd; skip it so
        // the test stays hermetic — the surface under test is the release
        // stage's URL derivation + the metadata write.
        ctx.options.skip_stages = vec!["changelog".to_string()];

        let log = anodizer_core::log::StageLogger::new(
            "publish-only-release-url-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let opts = RunOpts { dry_run: true };
        run_per_crate(
            &mut ctx,
            &config,
            &log,
            opts,
            dist_base.clone(),
            vec!["a".to_string(), "b".to_string()],
        )
        .expect("both per-crate dry-run iterations must complete");

        for name in ["a", "b"] {
            let body = std::fs::read_to_string(dist_base.join(name).join("metadata.json")).unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            let expected_tag = format!("{name}-v0.0.0");
            assert_eq!(
                json["tag"], expected_tag,
                "crate '{name}' metadata must carry its own tag"
            );
            assert_eq!(
                json["release_url"],
                format!("https://github.com/acme/widget/releases/tag/{expected_tag}"),
                "crate '{name}' metadata must carry its OWN release URL"
            );
        }
        assert!(
            ctx.template_vars().get("ReleaseURL").is_none(),
            "guard Drop must restore the caller's pre-loop (unset) ReleaseURL"
        );
    }

    /// `reset_release_url` must rewind `ReleaseURL` to the captured
    /// baseline at every iteration top, and the guard's Drop must restore
    /// it for the caller — otherwise a crate whose release stage never
    /// derives a URL (skipped stage, no resolvable repo) inherits the
    /// prior crate's URL into its metadata.json / announce templates.
    #[test]
    fn per_crate_overlay_guard_resets_release_url() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(ctx.template_vars().get("ReleaseURL").is_none());
        {
            let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
            // Simulate iteration 1's release stage setting the var.
            guard
                .ctx_mut()
                .set_release_url("https://github.com/acme/widget/releases/tag/a-v0.0.0");
            // Iteration 2's loop-top reset must rewind to the unset baseline.
            guard.reset_release_url();
            assert!(
                guard.ctx_mut().template_vars().get("ReleaseURL").is_none(),
                "loop-top reset must rewind ReleaseURL to the pre-loop baseline"
            );
            // Iteration 2 sets its own URL; Drop must still restore the baseline.
            guard
                .ctx_mut()
                .set_release_url("https://github.com/acme/widget/releases/tag/b-v0.0.0");
        }
        assert!(
            ctx.template_vars().get("ReleaseURL").is_none(),
            "guard Drop must restore the caller's pre-loop (unset) ReleaseURL"
        );
    }

    /// `PerCrateOverlayGuard::Drop` must fire on unwind so a panic from
    /// inside the iteration body (e.g. an `unwrap` deep in stage code,
    /// a templating overflow, an `unreachable!()`) still rolls the
    /// caller's `ctx` back to its pre-loop shape. The closure-then-
    /// restore pattern this RAII guard replaces would skip the restore
    /// on panic, leaking mid-iteration override values into any outer
    /// `catch_unwind` boundary (test harnesses, embedding crates).
    #[test]
    fn per_crate_overlay_guard_restores_on_panic() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let mut config = Config::default();
        let original_dist = std::path::PathBuf::from("/tmp/per-crate-guard-panic/dist");
        config.dist = original_dist.clone();
        let mut ctx = Context::new(config, ContextOptions::default());
        let original_selected = vec!["root-crate".to_string()];
        let original_skip = vec!["root-skip".to_string()];
        ctx.options.selected_crates = original_selected.clone();
        ctx.options.skip_stages = original_skip.clone();

        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
            // Simulate the per-iteration mutations the loop performs.
            let inner = guard.ctx_mut();
            inner.config.dist = std::path::PathBuf::from("/scratch/mid-iteration");
            inner.options.selected_crates = vec!["mid-iter-crate".to_string()];
            inner.options.skip_stages = vec!["mid-iter-skip".to_string()];
            // Panic before the guard would normally fall out of scope
            // at the end of the loop. The Drop impl must still fire.
            panic!("simulated mid-iteration panic");
        }));

        assert!(
            result.is_err(),
            "fixture must actually panic — otherwise the guard's restore would also \
             run via the happy path and the test would pass trivially"
        );
        assert_eq!(
            ctx.config.dist, original_dist,
            "Drop must restore ctx.config.dist on panic"
        );
        assert_eq!(
            ctx.options.selected_crates, original_selected,
            "Drop must restore ctx.options.selected_crates on panic"
        );
        assert_eq!(
            ctx.options.skip_stages, original_skip,
            "Drop must restore ctx.options.skip_stages on panic"
        );
    }

    /// Each per-crate iteration must apply its workspace overlay to a
    /// clean baseline. `apply_workspace_overlay` overwrites `changelog` /
    /// `signs` only when the workspace sets them and *appends* to `env`,
    /// so without the guard's per-iteration `reset_overlay_fields` a value
    /// set by workspace A would leak into workspace B (which leaves it
    /// unset) and `env` would accumulate A's entries every iteration.
    #[test]
    fn per_crate_overlay_does_not_leak_across_workspaces() {
        use anodizer_core::config::{
            ChangelogConfig, Config, CrateConfig, HookEntry, HooksConfig, WorkspaceConfig,
        };
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::signing::SignConfig;

        fn ws(name: &str, set_overlay: bool) -> WorkspaceConfig {
            WorkspaceConfig {
                name: name.to_string(),
                crates: vec![CrateConfig {
                    name: name.to_string(),
                    ..CrateConfig::default()
                }],
                changelog: set_overlay.then(|| ChangelogConfig {
                    format: Some(format!("{name}-format")),
                    ..ChangelogConfig::default()
                }),
                signs: if set_overlay {
                    vec![SignConfig {
                        id: Some(format!("{name}-sign")),
                        ..SignConfig::default()
                    }]
                } else {
                    Vec::new()
                },
                binary_signs: if set_overlay {
                    vec![SignConfig {
                        id: Some(format!("{name}-binary-sign")),
                        ..SignConfig::default()
                    }]
                } else {
                    Vec::new()
                },
                before: set_overlay.then(|| HooksConfig {
                    hooks: Some(vec![HookEntry::Simple(format!("{name}-before"))]),
                    post: None,
                }),
                after: set_overlay.then(|| HooksConfig {
                    hooks: Some(vec![HookEntry::Simple(format!("{name}-after"))]),
                    post: None,
                }),
                env: set_overlay.then(|| vec![format!("{name}_KEY=1")]),
                ..WorkspaceConfig::default()
            }
        }

        // Baseline config carries no changelog/signs/env so any value
        // observed after the overlay came from the workspace, not the
        // top-level config.
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let workspace_a = ws("alpha", /* set_overlay */ true);
        let workspace_b = ws("beta", /* set_overlay */ false);

        let mut guard = PerCrateOverlayGuard::capture(&mut ctx);

        // Iteration A: workspace alpha sets every overlay field.
        guard.reset_overlay_fields();
        crate::commands::helpers::apply_workspace_overlay(
            &mut guard.ctx_mut().config,
            &workspace_a,
        );
        {
            let cfg = &guard.ctx_mut().config;
            assert_eq!(
                cfg.changelog.as_ref().and_then(|c| c.format.as_deref()),
                Some("alpha-format")
            );
            assert_eq!(cfg.signs.len(), 1);
            assert_eq!(cfg.binary_signs.len(), 1);
            assert_eq!(
                cfg.before
                    .as_ref()
                    .and_then(|h| h.hooks.as_ref())
                    .map(|v| v.as_slice()),
                Some([HookEntry::Simple("alpha-before".to_string())].as_slice())
            );
            assert_eq!(
                cfg.after
                    .as_ref()
                    .and_then(|h| h.hooks.as_ref())
                    .map(|v| v.as_slice()),
                Some([HookEntry::Simple("alpha-after".to_string())].as_slice())
            );
            assert_eq!(
                cfg.env.as_deref(),
                Some(["alpha_KEY=1".to_string()].as_slice())
            );
        }

        // Iteration B: workspace beta leaves every overlay field unset, so
        // after the reset+overlay it must NOT inherit alpha's values, and
        // env must not have accumulated alpha's entry.
        guard.reset_overlay_fields();
        crate::commands::helpers::apply_workspace_overlay(
            &mut guard.ctx_mut().config,
            &workspace_b,
        );
        {
            let cfg = &guard.ctx_mut().config;
            assert!(
                cfg.changelog.is_none(),
                "workspace B must not inherit A's changelog"
            );
            assert!(
                cfg.signs.is_empty(),
                "workspace B must not inherit A's signs"
            );
            assert!(
                cfg.binary_signs.is_empty(),
                "workspace B must not inherit A's binary_signs"
            );
            assert!(
                cfg.before.is_none(),
                "workspace B must not inherit A's before hooks"
            );
            assert!(
                cfg.after.is_none(),
                "workspace B must not inherit A's after hooks"
            );
            assert!(
                cfg.env.as_ref().map(|e| e.is_empty()).unwrap_or(true),
                "env must not accumulate A's entries into B's iteration: {:?}",
                cfg.env
            );
        }

        // Drop must rewind the overlay fields back to the empty baseline.
        drop(guard);
        assert!(ctx.config.changelog.is_none());
        assert!(ctx.config.signs.is_empty());
        assert!(ctx.config.binary_signs.is_empty());
        assert!(ctx.config.before.is_none());
        assert!(ctx.config.after.is_none());
        assert!(
            ctx.config
                .env
                .as_ref()
                .map(|e| e.is_empty())
                .unwrap_or(true)
        );
    }

    // ── per-crate Tag restore (the lockstep-workspace title/changelog bug) ──

    mod per_crate_tag {
        use super::*;
        use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};
        use serial_test::serial;

        fn quiet_log() -> StageLogger {
            StageLogger::new("per-crate-tag-test", Verbosity::Quiet)
        }

        /// Run `body` with the process cwd swapped to a freshly-`git
        /// init`ed empty temp repo, restoring the original cwd after.
        ///
        /// `apply_per_crate_tag`'s `PreviousTag` lookup shells to `git
        /// describe` in the process cwd; without this the tag tests would
        /// scan the real anodize checkout (non-hermetic, slow, and
        /// dependent on whatever tags happen to be in the dev's tree). An
        /// empty repo makes the lookup return an error fast — caught and
        /// logged by `apply_per_crate_tag`, leaving `Tag` (the thing under
        /// test) untouched. Process-wide cwd swap, so callers must be
        /// `#[serial]`.
        fn with_hermetic_git_cwd(body: impl FnOnce()) {
            let tmp = tempfile::tempdir().unwrap();
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(tmp.path())
                    .status()
                    .expect("spawn git init")
                    .success(),
                "git init must succeed for the hermetic tag-test repo",
            );
            let orig = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            std::env::set_current_dir(orig).unwrap();
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        }

        fn crate_cfg(name: &str, tag_template: &str) -> CrateConfig {
            CrateConfig {
                name: name.to_string(),
                tag_template: tag_template.to_string(),
                ..CrateConfig::default()
            }
        }

        /// Build a config whose `crates` already hold the workspace's
        /// entries (the shape `apply_workspace_overlay` produces before
        /// `apply_per_crate_tag` runs).
        fn config_with_crates(crates: Vec<CrateConfig>) -> Config {
            Config {
                crates,
                ..Config::default()
            }
        }

        /// A lockstep workspace shares one `Version`; each crate's own
        /// `tag_template` must recover its own tag. cfgd's top-level
        /// crate templates `v{{ Version }}` → `v0.4.0`; cfgd-core
        /// templates `core-v{{ Version }}` → `core-v0.4.0`. Without the
        /// restore, both inherit whichever tag `resolve_git_context`
        /// pinned once at HEAD.
        #[test]
        #[serial]
        fn restores_per_crate_tag_from_tag_template() {
            with_hermetic_git_cwd(|| {
                for (crate_name, tag_template, expect_tag) in [
                    ("cfgd", "v{{ Version }}", "v0.4.0"),
                    ("cfgd-core", "core-v{{ Version }}", "core-v0.4.0"),
                    (
                        "cfgd-operator",
                        "operator-v{{ Version }}",
                        "operator-v0.4.0",
                    ),
                ] {
                    let config = config_with_crates(vec![crate_cfg(crate_name, tag_template)]);
                    let mut ctx = Context::new(config.clone(), ContextOptions::default());
                    ctx.template_vars_mut().set("Version", "0.4.0");
                    // The global, HEAD-derived tag every iteration would
                    // otherwise carry.
                    ctx.template_vars_mut().set("Tag", "core-v0.4.0");

                    apply_per_crate_tag(&mut ctx, &config, crate_name, &quiet_log());

                    assert_eq!(
                        ctx.template_vars().get("Tag").map(String::as_str),
                        Some(expect_tag),
                        "crate '{crate_name}' must carry its own tag, not the global HEAD tag",
                    );
                }
            });
        }

        /// Write a minimal preserved `context.json` recording only the
        /// `version`, under `<base>/<crate>/context.json`. Returns the
        /// per-crate dist subdir.
        fn write_preserved_version(
            base: &Path,
            crate_name: &str,
            version: &str,
        ) -> std::path::PathBuf {
            let crate_dist = base.join(crate_name);
            std::fs::create_dir_all(&crate_dist).unwrap();
            std::fs::write(
                crate_dist.join("context.json"),
                format!(r#"{{"version":"{version}","commit":"deadbeefcafe"}}"#),
            )
            .unwrap();
            crate_dist
        }

        /// Workspace per-crate INDEPENDENT-version mode: each crate's
        /// preserved manifest carries its OWN version, so the per-crate
        /// tag (and any version-templated artifact name / release title)
        /// must render against that crate's version, NOT the single
        /// HEAD-resolved global version. cfgd-core preserved at 0.5.1 and
        /// cfgd preserved at 0.4.0 — re-anchoring `Version` before the tag
        /// render recovers `core-v0.5.1` / `v0.4.0`. Without the
        /// `apply_per_crate_version` re-anchor, both render against the
        /// global `0.4.0` and the wrong crate gets a mis-tagged release.
        #[test]
        #[serial]
        fn independent_version_workspace_renders_per_crate_version() {
            with_hermetic_git_cwd(|| {
                let tmp = tempfile::tempdir().unwrap();
                let dist = tmp.path().join("dist");

                let cases = [
                    ("cfgd", "v{{ Version }}", "0.4.0", "v0.4.0"),
                    ("cfgd-core", "core-v{{ Version }}", "0.5.1", "core-v0.5.1"),
                ];
                for (crate_name, tag_template, preserved_version, expect_tag) in cases {
                    let crate_dist = write_preserved_version(&dist, crate_name, preserved_version);
                    let config = config_with_crates(vec![crate_cfg(crate_name, tag_template)]);
                    let mut ctx = Context::new(config.clone(), ContextOptions::default());
                    // The single HEAD-resolved global version every
                    // iteration would otherwise inherit.
                    ctx.template_vars_mut().set("Version", "0.4.0");
                    ctx.template_vars_mut().set("Tag", "v0.4.0");

                    apply_per_crate_version(&mut ctx, &crate_dist, crate_name, &quiet_log());
                    assert_eq!(
                        ctx.template_vars().get("Version").map(String::as_str),
                        Some(preserved_version),
                        "crate '{crate_name}' must carry its own preserved Version",
                    );

                    apply_per_crate_tag(&mut ctx, &config, crate_name, &quiet_log());
                    assert_eq!(
                        ctx.template_vars().get("Tag").map(String::as_str),
                        Some(expect_tag),
                        "crate '{crate_name}' tag must render against its own preserved version",
                    );
                }
            });
        }

        /// Write a minimal preserved `context.json` that records NO
        /// `version` (only a commit), under `<base>/<crate>/context.json`.
        /// Mirrors a preserved dist whose manifest predates the version
        /// field or was hand-written without it — the case where
        /// `apply_per_crate_version` early-returns.
        fn write_preserved_no_version(base: &Path, crate_name: &str) -> std::path::PathBuf {
            let crate_dist = base.join(crate_name);
            std::fs::create_dir_all(&crate_dist).unwrap();
            std::fs::write(
                crate_dist.join("context.json"),
                r#"{"version":"","commit":"deadbeefcafe"}"#,
            )
            .unwrap();
            crate_dist
        }

        /// Per-crate iteration must rewind the version-derived vars to the
        /// pre-loop baseline at the START of each iteration, mirroring the
        /// `baseline_skip_stages` reset. `apply_per_crate_version`
        /// early-returns (leaves the vars untouched) when a crate's
        /// preserved manifest records no version; without the per-iteration
        /// reset, crate 2 (no preserved version) would inherit crate 1's
        /// re-anchored version and render its tag against the WRONG value.
        ///
        /// Drives the real loop shape: capture the guard, then per crate
        /// `reset_version_vars()` → `apply_per_crate_version`. Crate 1
        /// preserves 0.5.1; crate 2 preserves no version and must fall back
        /// to the pre-loop baseline (0.4.0), NOT inherit crate 1's 0.5.1.
        #[test]
        fn per_iteration_reset_prevents_version_bleed_when_next_crate_lacks_version() {
            let tmp = tempfile::tempdir().unwrap();
            let dist = tmp.path().join("dist");
            let crate1_dist = write_preserved_version(&dist, "cfgd-core", "0.5.1");
            let crate2_dist = write_preserved_no_version(&dist, "cfgd");

            let mut ctx = Context::new(Config::default(), ContextOptions::default());
            // The single HEAD-resolved baseline every iteration rewinds to.
            ctx.template_vars_mut().set("Version", "0.4.0");
            ctx.template_vars_mut().set("Major", "0");
            ctx.template_vars_mut().set("Minor", "4");
            ctx.template_vars_mut().set("Patch", "0");

            let mut guard = PerCrateOverlayGuard::capture(&mut ctx);

            // Iteration 1: crate with a preserved version re-anchors to it.
            guard.reset_version_vars();
            apply_per_crate_version(guard.ctx_mut(), &crate1_dist, "cfgd-core", &quiet_log());
            assert_eq!(
                guard
                    .ctx_mut()
                    .template_vars()
                    .get("Version")
                    .map(String::as_str),
                Some("0.5.1"),
                "crate 1 must re-anchor to its own preserved version",
            );

            // Iteration 2: crate WITHOUT a preserved version. The
            // per-iteration reset must rewind to the baseline before the
            // early-returning `apply_per_crate_version`, so the vars are the
            // pre-loop 0.4.0 — NOT crate 1's leaked 0.5.1.
            guard.reset_version_vars();
            apply_per_crate_version(guard.ctx_mut(), &crate2_dist, "cfgd", &quiet_log());
            let vars = guard.ctx_mut();
            assert_eq!(
                vars.template_vars().get("Version").map(String::as_str),
                Some("0.4.0"),
                "crate 2 (no preserved version) must fall back to the pre-loop \
                 baseline, NOT inherit crate 1's re-anchored version",
            );
            assert_eq!(
                vars.template_vars().get("Major").map(String::as_str),
                Some("0"),
                "derived Major must also rewind to baseline, not crate 1's",
            );
        }

        /// A preserved version with prerelease + build metadata must
        /// populate the derived vars (`Major`/`Minor`/`Patch`/
        /// `Prerelease`/`BuildMetadata`) so version-templated names that
        /// reference them render with the per-crate values too.
        #[test]
        fn apply_per_crate_version_populates_derived_vars() {
            let tmp = tempfile::tempdir().unwrap();
            let crate_dist = write_preserved_version(tmp.path(), "cfgd", "1.2.3-rc.1+build.7");
            let mut ctx = Context::new(Config::default(), ContextOptions::default());

            apply_per_crate_version(&mut ctx, &crate_dist, "cfgd", &quiet_log());

            let v = ctx.template_vars();
            assert_eq!(
                v.get("Version").map(String::as_str),
                Some("1.2.3-rc.1+build.7")
            );
            assert_eq!(v.get("RawVersion").map(String::as_str), Some("1.2.3"));
            assert_eq!(v.get("Major").map(String::as_str), Some("1"));
            assert_eq!(v.get("Minor").map(String::as_str), Some("2"));
            assert_eq!(v.get("Patch").map(String::as_str), Some("3"));
            assert_eq!(v.get("Prerelease").map(String::as_str), Some("rc.1"));
            assert_eq!(v.get("BuildMetadata").map(String::as_str), Some("build.7"));
        }

        /// A missing preserved manifest (or a non-semver version) leaves
        /// the upstream `Version` untouched rather than blanking it.
        #[test]
        fn apply_per_crate_version_missing_manifest_leaves_version() {
            let tmp = tempfile::tempdir().unwrap();
            let mut ctx = Context::new(Config::default(), ContextOptions::default());
            ctx.template_vars_mut().set("Version", "9.9.9");

            apply_per_crate_version(
                &mut ctx,
                &tmp.path().join("absent-crate"),
                "absent-crate",
                &quiet_log(),
            );

            assert_eq!(
                ctx.template_vars().get("Version").map(String::as_str),
                Some("9.9.9"),
                "a missing preserved manifest must not clobber the upstream Version",
            );
        }

        /// The overlay guard must snapshot the pre-loop version-derived
        /// vars and restore them on drop so the per-iteration re-anchor
        /// never leaks into the caller's context.
        #[test]
        fn overlay_guard_restores_version_vars() {
            let mut ctx = Context::new(Config::default(), ContextOptions::default());
            ctx.template_vars_mut().set("Version", "0.4.0");
            ctx.template_vars_mut().set("Major", "0");

            {
                let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
                let inner = guard.ctx_mut();
                inner.template_vars_mut().set("Version", "0.5.1");
                inner.template_vars_mut().set("Major", "9");
            }

            assert_eq!(
                ctx.template_vars().get("Version").map(String::as_str),
                Some("0.4.0"),
                "Drop must restore the caller's Version",
            );
            assert_eq!(
                ctx.template_vars().get("Major").map(String::as_str),
                Some("0"),
                "Drop must restore the caller's Major",
            );
        }

        /// The crate may live in `config.workspaces` rather than the
        /// top-level `crates` list (e.g. when the caller passes the
        /// original config rather than the overlaid one). The lookup
        /// must fall back to the workspace list.
        #[test]
        #[serial]
        fn finds_tag_template_in_workspace_fallback() {
            with_hermetic_git_cwd(|| {
                let config = Config {
                    workspaces: Some(vec![WorkspaceConfig {
                        name: "cfgd".to_string(),
                        crates: vec![crate_cfg("cfgd", "v{{ Version }}")],
                        ..WorkspaceConfig::default()
                    }]),
                    ..Config::default()
                };
                let mut ctx = Context::new(config.clone(), ContextOptions::default());
                ctx.template_vars_mut().set("Version", "0.4.0");
                ctx.template_vars_mut().set("Tag", "core-v0.4.0");

                apply_per_crate_tag(&mut ctx, &config, "cfgd", &quiet_log());

                assert_eq!(
                    ctx.template_vars().get("Tag").map(String::as_str),
                    Some("v0.4.0"),
                    "workspace-list fallback must resolve the crate's tag_template",
                );
            });
        }

        /// A crate with no matching config / empty `tag_template` leaves
        /// the upstream tag untouched rather than blanking it.
        #[test]
        fn missing_tag_template_leaves_tag_untouched() {
            let config = config_with_crates(vec![crate_cfg("known", "v{{ Version }}")]);
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            ctx.template_vars_mut().set("Version", "0.4.0");
            ctx.template_vars_mut().set("Tag", "v0.4.0");

            apply_per_crate_tag(&mut ctx, &config, "unknown-crate", &quiet_log());

            assert_eq!(
                ctx.template_vars().get("Tag").map(String::as_str),
                Some("v0.4.0"),
                "an unmatched crate must not clobber the existing Tag",
            );
        }

        /// The overlay guard must snapshot the pre-loop `Tag` /
        /// `PreviousTag` and restore them on drop so the per-iteration
        /// re-derivation never leaks into the caller's context.
        #[test]
        fn overlay_guard_restores_tag_and_previous_tag() {
            let config = Config {
                dist: std::path::PathBuf::from("/tmp/per-crate-guard-tag/dist"),
                ..Config::default()
            };
            let mut ctx = Context::new(config, ContextOptions::default());
            ctx.template_vars_mut().set("Tag", "v0.4.0");
            ctx.template_vars_mut().set("PreviousTag", "v0.3.0");

            {
                let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
                let inner = guard.ctx_mut();
                inner.template_vars_mut().set("Tag", "core-v0.4.0");
                inner.template_vars_mut().set("PreviousTag", "core-v0.3.0");
            }

            assert_eq!(
                ctx.template_vars().get("Tag").map(String::as_str),
                Some("v0.4.0"),
                "Drop must restore the caller's Tag",
            );
            assert_eq!(
                ctx.template_vars().get("PreviousTag").map(String::as_str),
                Some("v0.3.0"),
                "Drop must restore the caller's PreviousTag",
            );
        }

        /// `write_metadata_json` must land `metadata.json` under
        /// `ctx.config.dist` (the per-crate subdir the loop re-anchored
        /// to), NOT the flat `config.dist` the `config` param still
        /// carries. The release stage's existence gate reads
        /// `ctx.config.dist/metadata.json`; writing to the flat root
        /// would leave that gate looking at a missing file and bail
        /// before the draft→published PATCH.
        ///
        /// This mirrors the real `run_per_crate` / `run_one_crate_dist`
        /// call shape: `config.dist` is the workspace-root dist, while
        /// `ctx.config.dist` was re-anchored onto `dist/<crate>/`. Asserts
        /// the file materializes under `ctx.config.dist` and NOT under
        /// the flat root — fails against the pre-fix code that derived the
        /// dir from the `config` param.
        #[test]
        fn write_metadata_json_materializes_per_crate_metadata() {
            let tmp = tempfile::tempdir().unwrap();
            let flat_dist = tmp.path().join("dist");
            let crate_dist = flat_dist.join("cfgd-core");

            // `config` carries the FLAT dist root (what the loop threads
            // through unchanged), `ctx.config.dist` the per-crate subdir.
            let config = Config {
                project_name: "cfgd".to_string(),
                dist: flat_dist.clone(),
                crates: vec![crate_cfg("cfgd-core", "core-v{{ Version }}")],
                ..Config::default()
            };
            let ctx_config = Config {
                dist: crate_dist.clone(),
                ..config.clone()
            };
            let mut ctx = Context::new(ctx_config, ContextOptions::default());
            ctx.template_vars_mut().set("Version", "0.4.0");
            ctx.template_vars_mut().set("Tag", "core-v0.4.0");
            ctx.template_vars_mut().set("FullCommit", "deadbeef");
            ctx.set_release_url("https://github.com/acme/cfgd/releases/tag/core-v0.4.0");

            let path =
                crate::commands::helpers::write_metadata_json(&ctx, &config, &quiet_log()).unwrap();

            assert_eq!(
                path,
                crate_dist.join("metadata.json"),
                "metadata.json must land under ctx.config.dist (per-crate subdir)",
            );
            assert!(
                path.exists(),
                "metadata.json must exist for the release upload"
            );
            assert!(
                !flat_dist.join("metadata.json").exists(),
                "metadata.json must NOT land at the flat root (where the release \
                 stage never looks in per-crate mode)",
            );
            let body = std::fs::read_to_string(&path).unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                json["tag"], "core-v0.4.0",
                "metadata must carry the per-crate tag"
            );
            assert_eq!(json["version"], "0.4.0");
            assert_eq!(json["project_name"], "cfgd");
            assert_eq!(
                json["release_url"], "https://github.com/acme/cfgd/releases/tag/core-v0.4.0",
                "per-crate metadata must carry this crate's own release URL \
                 (the action-side `release-url` output reads `.release_url`)"
            );
        }
    }

    // ── --crate dispatch: per-crate-subdir layout awareness ────

    fn subdir_test_log() -> StageLogger {
        StageLogger::new("subdir-test", anodizer_core::log::Verbosity::Quiet)
    }

    #[test]
    fn crate_subdir_has_manifest_detects_context_json() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("cfgd");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("context.json"), "{}").unwrap();
        assert!(
            crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
            "a subdir with context.json must be recognized",
        );
    }

    #[test]
    fn crate_subdir_has_manifest_detects_sharded_context() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("cfgd");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("context-linux.json"), "{}").unwrap();
        assert!(
            crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
            "a subdir with a sharded context-<shard>.json must be recognized",
        );
    }

    #[test]
    fn crate_subdir_has_manifest_false_for_flat_layout() {
        let tmp = tempfile::tempdir().unwrap();
        // Flat layout: manifest at the root, no per-crate subdir.
        std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
        assert!(
            !crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
            "absence of dist/<crate>/ must fall back to flat (returns false)",
        );
    }
}
