use super::*;
use crate::commands::helpers;
use crate::pipeline;
use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::git::short_commit_str;
use anodizer_core::log::StageLogger;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Which per-crate lifecycle hook block to run.
#[derive(Clone, Copy)]
pub(super) enum HookKind {
    Before,
    After,
}

impl HookKind {
    /// Label used in hook output (`ran <label> hook`) and in the `--skip`
    /// gate. Matches the top-level hook labels so per-crate and global hooks
    /// read identically in logs.
    fn label(self) -> &'static str {
        match self {
            HookKind::Before => "before",
            HookKind::After => "after",
        }
    }
}

/// Fire a crate's per-crate `before:` / `after:` lifecycle hooks (resolved
/// from the crate's RESOLVED [`CrateConfig`] after the workspace overlay is
/// applied) scoped to the crate's already-anchored template vars.
///
/// Honors `--skip=before` / `--skip=after` exactly like the top-level hooks
/// so an operator can suppress both surfaces with one flag. A crate with no
/// matching hook block is a no-op. The top-level `before:` / `after:` hooks
/// fire separately (once per release in the outer dispatcher) — this only
/// adds the per-crate surface.
pub(super) fn run_per_crate_lifecycle_hooks(
    ctx: &Context,
    crate_name: &str,
    kind: HookKind,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let label = kind.label();
    if ctx.should_skip(label) {
        return Ok(());
    }
    let Some(crate_cfg) = ctx.config.find_crate(crate_name) else {
        return Ok(());
    };
    let block = match kind {
        HookKind::Before => crate_cfg.before.as_ref(),
        HookKind::After => crate_cfg.after.as_ref(),
    };
    let Some(hooks) = block
        .and_then(|b| b.hooks.as_ref())
        .filter(|h| !h.is_empty())
    else {
        return Ok(());
    };
    pipeline::run_hooks(
        hooks,
        label,
        anodizer_core::hooks::HookRunContext::new(dry_run, log, Some(ctx.template_vars())),
    )
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
pub(super) struct PerCrateOverlayGuard<'a> {
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
pub(super) const VERSION_TEMPLATE_VARS: &[&str] = &[
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
pub(super) struct OverlayFields {
    crates: Vec<anodizer_core::config::CrateConfig>,
    workspaces: Option<Vec<anodizer_core::config::WorkspaceConfig>>,
    changelog: Option<anodizer_core::config::ChangelogConfig>,
    signs: Vec<anodizer_core::config::SignConfig>,
    binary_signs: Vec<anodizer_core::config::SignConfig>,
    before: Option<anodizer_core::config::HooksConfig>,
    after: Option<anodizer_core::config::HooksConfig>,
    env: Option<Vec<String>>,
}

impl OverlayFields {
    pub(super) fn capture(config: &Config) -> Self {
        Self {
            crates: config.crates.clone(),
            workspaces: config.workspaces.clone(),
            changelog: config.changelog.clone(),
            signs: config.signs.clone(),
            binary_signs: config.binary_signs.clone(),
            before: config.before.clone(),
            after: config.after.clone(),
            env: config.env.clone(),
        }
    }

    pub(super) fn restore_into(&self, config: &mut Config) {
        config.crates = self.crates.clone();
        config.workspaces = self.workspaces.clone();
        config.changelog = self.changelog.clone();
        config.signs = self.signs.clone();
        config.binary_signs = self.binary_signs.clone();
        config.before = self.before.clone();
        config.after = self.after.clone();
        config.env = self.env.clone();
    }
}

impl<'a> PerCrateOverlayGuard<'a> {
    pub(super) fn capture(ctx: &'a mut Context) -> Self {
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
    pub(super) fn reset_overlay_fields(&mut self) {
        let saved = self.saved_overlay.clone();
        saved.restore_into(&mut self.ctx.config);
    }

    /// Per-iteration `skip_stages` reset baseline. Returns the snapshot
    /// the guard took at capture-time so the loop can rewind to the
    /// pre-overlay value before applying the current workspace's
    /// `skip:` list.
    pub(super) fn snapshot_skip_stages(&self) -> &[String] {
        &self.saved_skip_stages
    }

    /// Rewind the version-derived template vars to the captured baseline.
    /// Call at the start of each iteration *before* `apply_per_crate_version`
    /// so a version re-anchored for a prior crate can't leak into one whose
    /// preserved manifest records no version (where `apply_per_crate_version`
    /// early-returns and leaves the vars untouched). Mirrors the
    /// `baseline_skip_stages` reset; the Drop-restore at loop end still
    /// returns the caller's pre-loop values.
    pub(super) fn reset_version_vars(&mut self) {
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
    pub(super) fn reset_release_url(&mut self) {
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
    pub(super) fn ctx_mut(&mut self) -> &mut Context {
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
pub(super) fn apply_per_crate_version(
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
pub(super) fn peek_preserved_version(crate_dist: &Path) -> Option<String> {
    let contexts = discover_preserved_contexts(crate_dist).ok()?;
    contexts
        .into_iter()
        .map(|(_, c)| c.version)
        .find(|v| !v.is_empty())
}

pub(super) fn apply_per_crate_tag(
    ctx: &mut Context,
    config: &Config,
    crate_name: &str,
    log: &StageLogger,
) {
    // The crate's own raw template if set, else the `{name}-v` convention
    // (NOT `resolved_tag_template()`'s built-in `v{{ Version }}` default,
    // which is the wrong family for per-crate `{name}-v` configs). The
    // `crate_prefix` derived below extracts from this SAME resolved
    // template, keeping the re-anchored `Tag` and `PreviousTag` in the
    // same family.
    let tag_template = config
        .find_crate(crate_name)
        .map(|c| {
            c.tag_template
                .clone()
                .unwrap_or_else(|| format!("{crate_name}-v{{{{ Version }}}}"))
        })
        .filter(|t| !t.is_empty());
    let Some(tag_template) = tag_template else {
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
pub(super) fn merge_workspace_skip(into: &mut Vec<String>, ws_skip: &[String]) {
    for stage in ws_skip {
        if !into.iter().any(|s| s == stage) {
            into.push(stage.clone());
        }
    }
}

/// Inner body of the publish-only pipeline for a single dist root.
/// Called by both `run()` (flat layout) and `run_per_crate()` (per-crate layout).
pub(super) fn run_one_crate_dist(
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
    //   * Directory-bundle artifacts (macOS `.app`, `format=appbundle`) —
    //     a `.app` is a DIRECTORY, not a file. dmg/pkg wrap it into a
    //     `.dmg`/`.pkg` and every file subject (checksum/sign/upload)
    //     filters it out via `is_directory_bundle_artifact`; the presence
    //     check must share that classifier, else a legitimately-preserved
    //     `.app` directory fails the `is_file()` probe and reads as
    //     "missing". Same classifier — not a runtime `is_dir()` probe,
    //     which would misbehave under dry-run before the dir is materialized.
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
                ) && !anodizer_core::artifact::is_directory_bundle_artifact(a)
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
    // usual release / blob / publish / snapcraft-publish chain — the
    // head SignStage is the production-keys re-sign pass that overlays
    // shippable signatures on the byte-stable preserved archives.
    // Distinct from `build_publish_pipeline` (consumed by `anodize
    // publish`) which does NOT prepend SignStage; conflating them
    // would silently introduce a new credential requirement to
    // `anodize publish`.
    let p = pipeline::build_publish_only_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        super::super::run_post_pipeline(ctx, config, opts.dry_run, log)?;

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
        super::super::gate_required_failures(ctx)?;
    }

    result
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
/// still match. `ChecksumStage` nonetheless runs in the publish-only
/// pipeline right after the head `SignStage`
/// (see `pipeline::builders::build_publish_only_pipeline`): the
/// byte-deterministic recompute refreshes the checksum manifest over
/// the production-signed tree and backfills the `sha256` metadata
/// that the determinism-stripped `artifacts.json` omits.
pub(super) fn strip_ephemeral_signatures(ctx: &mut Context, log: &StageLogger) {
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
pub(super) fn detect_duplicate_artifact_paths(ctx: &Context) -> Result<()> {
    crate::commands::helpers::detect_duplicate_paths(
        ctx.artifacts.all().iter().map(|a| a.path.as_path()),
    )
}
