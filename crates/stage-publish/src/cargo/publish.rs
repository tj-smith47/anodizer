//! Publish orchestration: the guarded `cargo publish` loop and its
//! changelog-provenance and clean-tree preconditions.

use super::*;

/// Whether the version-bump commit that LAST touched this crate's own
/// `CHANGELOG.md` records that anodizer itself regenerated it for
/// `crate_name` at `version` — the provenance under which a crate-root
/// `CHANGELOG.md` difference against an already-published version is the
/// tool's own re-cut artifact rather than operator-authored drift (see
/// [`crates_equal_modulo_vcs`]).
///
/// The marker is written by the `tag` / `bump --commit` `--changelog` refresh
/// into the bump commit body (see
/// [`anodizer_core::git::changelog_regenerated_marker`]), so it travels with
/// the repository to any later publish run — including a `--publish-only`
/// re-cut whose own invocation skips the changelog stage. Keying on the
/// FILE's provenance (who last wrote it, for which version) rather than this
/// run's changelog stage config closes both failure modes of the config
/// proxy: a `--skip=changelog` / `use: github-native` publish run no longer
/// dead-ends on the tool's own regeneration, and an active changelog stage
/// no longer forgives drift the tool never produced. Anchoring on the file's
/// LAST toucher (not any marker in history) means an operator hand-edit
/// committed after the regeneration withdraws the forgiveness.
///
/// `changelog_rel_path` is the crate's own `CHANGELOG.md`, repo-relative.
/// A git failure counts as "no provenance" (fail closed: the guard stays
/// byte-strict on `CHANGELOG.md` when it cannot prove the tool wrote it) and
/// is warned at default visibility, since it changes the guard's stance — a
/// shallow clone that cannot see the bump commit hard-fails on drift the tool
/// authored.
pub(crate) fn changelog_provenance_recorded(
    ctx: &Context,
    crate_name: &str,
    version: &str,
    changelog_rel_path: &str,
    log: &StageLogger,
) -> bool {
    let repo = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match anodizer_core::git::changelog_regenerated_recorded_in(
        &repo,
        crate_name,
        version,
        changelog_rel_path,
    ) {
        Ok(recorded) => recorded,
        Err(e) => {
            log.warn(&format!(
                "could not consult git history for the changelog provenance marker of \
                 '{crate_name}-{version}' ({e:#}); treating CHANGELOG.md as byte-strict — \
                 a CHANGELOG.md difference against the published version will hard-fail"
            ));
            false
        }
    }
}

/// Repo-relative path of a crate's own `CHANGELOG.md`, derived from its
/// configured `path` (`"."`/empty = the repo root). `/`-separated, matching
/// how git and the bump commit's write set address the file.
pub(crate) fn crate_changelog_rel_path(crate_path: &str) -> String {
    let dir = crate_path.trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        "CHANGELOG.md".to_string()
    } else {
        format!("{dir}/CHANGELOG.md")
    }
}

/// Refuse to run the content-vs-version poison guard against a dirty working
/// tree.
///
/// `cargo package` stamps `"dirty": true` into the `.crate`'s
/// `.cargo_vcs_info.json` whenever the tree differs from `HEAD`, which changes
/// the tarball bytes. The release (Publish) job checks out the committed tag —
/// a clean tree — so reproduction holds there. A manual `--publish-only` from a
/// DIRTY operator workspace would package dirty bytes and (a) false-poison a
/// crate that was published clean, or (b) mask real content drift behind the
/// dirty marker. Either way the comparison against the immutable index cksum is
/// no longer trustworthy.
///
/// Called ONCE before the publish loop's first binstall write, so anodizer's
/// own (expected) binstall mutation is not itself flagged. Fails loud rather
/// than silently skipping (a poison hole) or hard-failing on content (which
/// would misattribute the divergence to a code change). The message lists the
/// dirty paths and prescribes re-running from a clean tag checkout.
fn ensure_publish_tree_clean(ctx: &Context) -> Result<()> {
    let repo = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let porcelain = match anodizer_core::git::git_status_porcelain_result_in(&repo) {
        Ok(out) => out,
        // An errored `git status` (non-repo cwd, git absent, locked index)
        // cannot PROVE the tree is clean. A guard that "fails loud rather than
        // silently skipping" must refuse here, never treat the indeterminate
        // result as clean — that would be the very poison hole this gate closes.
        Err(e) => anyhow::bail!(
            "publish: cannot verify the working tree is clean before checking already-published \
             crates against the crates.io index ({e:#}). Without a clean-tree proof, a local \
             `cargo package` checksum cannot be trusted to match what was published from the \
             release tag. Re-run the publish from a clean git checkout of the release tag (the \
             Release job does this automatically)."
        ),
    };
    if porcelain.trim().is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "publish: working tree is DIRTY before verifying already-published crates against the \
         crates.io index. `cargo package` records the dirty state in the .crate, so a local \
         checksum would NOT match what was published from the clean release tag — \
         already-published content verification is unreliable. Re-run from a clean checkout of \
         the release tag (the Release job does this automatically; `git status` must show no \
         changes). Uncommitted changes:\n{porcelain}"
    );
}

/// Publish every eligible crate, in topological order, recording each
/// crate's published identity into `record` AT THE MOMENT its
/// `cargo publish` succeeds.
///
/// `record` is the authoritative rollback source: the publisher's
/// `rollback()` yanks exactly the crates appended here, so a publish that
/// succeeds on crate A then fails on crate B (returning `Err`) still
/// leaves A in `record` for the unwind. Crates skipped as
/// already-published — or by `skip:` / `if:` — are intentionally NOT
/// recorded: this run didn't publish them, so yanking them would revert a
/// prior run's (or someone else's) live release.
pub fn publish_to_cargo(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
) -> Result<()> {
    // Resolve the workspace-level auth decision ONCE, before the publish loop.
    // `Some(token)` means Trusted Publishing minted a short-lived crates.io
    // token to inject via `CARGO_REGISTRY_TOKEN` for every crate; `None` keeps
    // today's ambient-env behavior byte-identical (the token/Auto-with-token
    // path). Skipped in dry-run and under `--skip=cargo`: neither reaches a
    // real publish, so there is nothing to mint (and no network round-trip).
    let retry_policy = ctx.retry_policy();
    let minted = if ctx.is_dry_run() || ctx.should_skip("cargo") {
        None
    } else {
        resolve_workspace_cargo_token(ctx, &retry_policy, log)?
    };

    // Overlay the minted token onto the context as `CARGO_REGISTRY_TOKEN` for
    // the publish+rollback lifecycle. The child `cargo publish` still reads the
    // token via the explicit per-child `registry_token` thread below (it does
    // not consult `ctx.env_source()`), but the overlay makes the token visible
    // to the rollback dispatcher's scope-availability gate — so a partial OIDC
    // publish can still yank, even though no ambient token exists. Under
    // `auth: token` (ambient), `minted` is `None` and the context is untouched.
    if let Some(ref token) = minted {
        ctx.begin_cargo_trusted_publishing(token.clone());
    }

    let result = publish_to_cargo_with(
        ctx,
        selected,
        log,
        record,
        is_already_published,
        minted.as_deref(),
    );

    // Revoke timing for a minted (OIDC) token:
    // - success, or failure with NOTHING published → revoke + restore the base
    //   env source now; no rollback yank will run.
    // - failure with crates already live (non-empty `record`, the same signal
    //   `programmatic_rollback_on_failure` keys on) → LEAVE the overlay and the
    //   marker in place so `rollback()` can yank with the token and revoke it
    //   afterward.
    // A `?` between mint and this decision would leak the token past its
    // release; there is none — the loop's error is captured in `result`.
    if minted.is_some() {
        let published_something = !record.is_empty();
        let defer_for_rollback = result.is_err() && published_something;
        if !defer_for_rollback && let Some(token) = ctx.end_cargo_trusted_publishing() {
            oidc::revoke_trusted_publishing_token(&token, &retry_policy, log);
        }
    }
    result
}

/// Resolve the workspace-level crates.io credential decision for the run,
/// mirroring pypi's `resolve_upload_credential` collapsed to one
/// per-workspace choice (the minted token authorizes every crate matching the
/// repo's Trusted Publisher, so it is resolved once and reused).
///
/// Returns `Some(minted)` only when Trusted Publishing actually minted a
/// token; `None` for the token/ambient path, which must stay behaviorally
/// identical to today (inherit the ambient `CARGO_REGISTRY_TOKEN`, no
/// injection, no revoke).
///
/// * `auth: token` → `None` (ambient `CARGO_REGISTRY_TOKEN`, as today).
/// * `auth: oidc` → mint (strict); error if no OIDC context, or if the block
///   targets a non-crates.io registry (which has no Trusted-Publishing
///   contract).
/// * `auth: auto` → ambient token present → `None`; else OIDC context present
///   → mint; else a hard error naming both paths.
pub(crate) fn resolve_workspace_cargo_token(
    ctx: &Context,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Option<String>> {
    use anodizer_core::config::CargoAuthMode;

    // Active cargo configs across the crate universe (skip:/if: gated out AND
    // `selected_crates`-scoped — routed through the same `active_cargo_configs`
    // SSOT the publisher trait methods use, so a deselected sibling's block
    // (e.g. `--crate x` with an out-of-scope `y` on `auth: oidc`) can never
    // leak into this workspace-wide credential decision), paired with whether
    // each targets crates.io (Trusted Publishing has no custom-registry variant).
    let active: Vec<(&CargoPublishConfig, bool)> = super::publisher::active_cargo_configs(ctx)
        .into_iter()
        .map(|cargo| (cargo, targets_crates_io(Some(cargo))))
        .collect();
    if active.is_empty() {
        return Ok(None);
    }

    let ambient_token_present = ctx
        .env_source()
        .var("CARGO_REGISTRY_TOKEN")
        .is_some_and(|t| !t.is_empty());
    let oidc_available = oidc::oidc_context_available(ctx);

    // A strict `oidc` block against a non-crates.io registry cannot be honored
    // — Trusted Publishing only mints crates.io tokens. Fail loud rather than
    // silently minting an unusable token or falling back to a stored one.
    for (cargo, is_crates_io) in &active {
        if cargo.resolved_auth() == CargoAuthMode::Oidc && !is_crates_io {
            anyhow::bail!(
                "cargo: auth=oidc (Trusted Publishing) is only supported against crates.io, \
                 not the custom registry '{}' — use a token for a custom registry",
                cargo
                    .registry
                    .as_deref()
                    .or(cargo.index.as_deref())
                    .unwrap_or("<custom>")
            );
        }
    }

    // Strict OIDC anywhere forces a mint (the run refuses to fall back to a
    // token). Otherwise Auto decides per the ambient-token / OIDC-context
    // ladder. Token-mode never mints. The workspace is assumed uniform, so a
    // single decision covers every crate.
    let any_strict_oidc = active
        .iter()
        .any(|(cargo, _)| cargo.resolved_auth() == CargoAuthMode::Oidc);
    let any_auto = active
        .iter()
        .any(|(cargo, _)| cargo.resolved_auth() == CargoAuthMode::Auto);

    if any_strict_oidc {
        return oidc::mint_trusted_publishing_token(ctx, policy, log).map(Some);
    }
    if any_auto {
        if ambient_token_present {
            return Ok(None);
        }
        if oidc_available {
            return oidc::mint_trusted_publishing_token(ctx, policy, log).map(Some);
        }
        anyhow::bail!(
            "cargo: no credential available to publish to crates.io. Set \
             $CARGO_REGISTRY_TOKEN, or run under GitHub Actions with `id-token: write` and a \
             registered Trusted Publisher (auth: oidc / auto)."
        );
    }
    // Every active block is `auth: token` — the ambient token path, unchanged.
    Ok(None)
}

/// Test seam for [`publish_to_cargo`] that injects only the crates.io
/// already-published index check; the content-vs-version guard's local
/// `.crate` checksum is wired to the production [`local_crate_cksum`].
///
/// Production passes [`is_already_published`] (a real sparse-index GET);
/// tests pass a stub so the partial-failure rollback path can be exercised
/// without a network round-trip. The signature mirrors `is_already_published`
/// `(name, version, policy) -> Result<Option<cksum>>`.
pub(crate) fn publish_to_cargo_with(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
    already_published_check: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
        &StageLogger,
    ) -> Result<Option<String>>,
    registry_token: Option<&str>,
) -> Result<()> {
    publish_to_cargo_with_guard(
        ctx,
        selected,
        log,
        record,
        already_published_check,
        |name, crate_cfg, cargo_cfg| local_crate_cksum(name, crate_cfg, cargo_cfg, log),
        &anodizer_core::crate_scope::resolve_crate_tag,
        fetch_published_crate,
        registry_token,
    )
}

/// Full test seam: both the crates.io already-published index check AND the
/// content-vs-version guard's local `.crate` checksum computer are injected.
///
/// The local-cksum stub returns `(crate_name, crate_cfg, cargo_cfg) ->
/// Result<Option<LocalCrate>>`:
/// - `Ok(Some(LocalCrate))` — the local `.crate` sha256 + bytes the guard
///   compares against the index-recorded `cksum` (fast path) and, on
///   mismatch, against the fetched published `.crate` (slow path).
/// - `Ok(None)` — guard inapplicable (non-crates.io registry); the
///   already-published skip is also suppressed for that crate.
/// - `Err(_)` — local digest uncomputable; the guard FAILS CLOSED rather
///   than treat an unverifiable already-published version as a safe skip.
///
/// `fetch_published` mirrors `already_published_check`'s injection pattern —
/// production wires [`fetch_published_crate`] (a real crates.io static-CDN
/// GET), tests inject a stub so the slow path (only reached when the local
/// and index cksums disagree) can be exercised without a network round-trip.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(crate) fn publish_to_cargo_with_guard(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
    already_published_check: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
        &StageLogger,
    ) -> Result<Option<String>>,
    local_cksum_check: impl Fn(
        &str,
        &CrateConfig,
        Option<&CargoPublishConfig>,
    ) -> Result<Option<LocalCrate>>,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
    fetch_published: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
        &StageLogger,
    ) -> Result<Vec<u8>>,
    // Registry token override for each spawned `cargo publish`: `Some` only
    // when Trusted Publishing minted a short-lived crates.io token, injected as
    // `CARGO_REGISTRY_TOKEN` on the child. `None` inherits the ambient env
    // unchanged (the token/Auto-with-token path).
    registry_token: Option<&str>,
) -> Result<()> {
    // Defensive guard: the `--skip=cargo` gate lives in the
    // dispatcher in `lib.rs::PublishStage::run` so every publisher emits its
    // skip log uniformly. Re-checking here protects future direct callers
    // (tests, CLI sub-commands) from accidentally bypassing the gate. No log
    // is emitted on this path — the dispatcher already logged it.
    if ctx.should_skip("cargo") {
        return Ok(());
    }
    // Resolve the eligible publish set once — transitive-dep expansion,
    // `skip:`/`if:` gating, and topological ordering all live in
    // `cargo_publish_plan`, shared with the publish-simulation preflight so the
    // two can never disagree about which crates publish or in what order.
    let plan = cargo_publish_plan(ctx, selected, log)?;
    let CargoPublishPlan {
        order: sorted_names,
        cfgs: cargo_cfgs,
        versions: crate_versions,
        all_crates,
    } = plan;

    if sorted_names.is_empty() {
        // The publisher wrapper (`CargoPublisher::run`) emits the canonical
        // operator-facing warn for the no-eligible-crates path; this
        // branch is unreachable in normal dispatch because the wrapper
        // short-circuits before calling here, but defensive callers
        // (tests, direct CLI sub-commands) still exit cleanly.
        return Ok(());
    }

    // Build a quick lookup: name → depends_on
    let deps_map: HashMap<String, Vec<String>> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    if ctx.is_dry_run() {
        for name in &sorted_names {
            log.verbose(&run_per_crate_start_message(name));
            let cmd = publish_command(name, cargo_cfgs.get(name));
            log.status(&format!("(dry-run) would run: {}", cmd.join(" ")));
            // Surface that the content-vs-version poison guard would run for
            // any crate already on crates.io — operators see WHAT would be
            // checked without a network round-trip or local package step.
            if targets_crates_io(cargo_cfgs.get(name)) {
                log.status(&format!(
                    "(dry-run) would verify '{}' local .crate checksum against the crates.io index if already published",
                    name
                ));
            }
        }
        return Ok(());
    }

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every crate's index-check GET. Mirrors the per-pipe-invocation
    // pattern used by artifactory/cloudsmith.
    let retry_policy = ctx.retry_policy();

    // Hard backstop, BEFORE the first irreversible `cargo publish`: refuse to
    // start when any crate in the publish set has a workspace-internal
    // (non-dev) dependency that is neither in the set nor already on
    // crates.io. The publish-simulation preflight runs the same guard earlier
    // for a louder/earlier abort, but it is gated behind `--no-preflight`;
    // re-running it here means no real-publish path (publish_to_cargo /
    // --publish-only) can bypass it. Cheap: at most one sparse-index GET per
    // out-of-set dep, and a no-op for the common lockstep case where every
    // workspace dep is in the set. (Skipped in dry-run — the early return
    // above already handled that path.)
    //
    // The index probe routes through the SAME injected `already_published_check`
    // seam the publish loop uses, so the guard shares one mockable index path:
    // `Ok(Some)` = present, `Ok(None)` = positively absent, `Err` = inconclusive
    // (never fails the guard).
    {
        let probe = |name: &str, version: &str| match already_published_check(
            name,
            version,
            &retry_policy,
            log,
        ) {
            Ok(Some(_)) => DepIndexState::Present,
            Ok(None) => DepIndexState::Absent,
            Err(_) => DepIndexState::Unknown,
        };
        check_publish_set_completeness(&sorted_names, &all_crates, &crate_versions, &probe, log)?;
    }

    // Path lookup for the wait-for-workspace-deps manifest scan below.
    let crate_paths: HashMap<String, String> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.path.clone()))
        .collect();

    // Workspace-root dep map shared across the per-crate manifest scans —
    // parsed at most once per run.
    let mut ws_root_cache = RootDepCache::new();

    // Working-tree cleanliness gate — ONCE, before the loop's first binstall
    // write dirties the tree. Checked here (not per crate) because the binstall
    // mutation for crate A would otherwise dirty the tree and false-trip the
    // check for crate B in a multi-crate workspace. A dirty tree at entry means
    // `cargo package` stamps `"dirty": true` into `.cargo_vcs_info.json`,
    // changing the `.crate` bytes vs the clean tag checkout the original release
    // published from — so the content-vs-index comparison is unreliable (false
    // poison on a clean-published crate, or masking real drift). Fail loud
    // rather than skip (a poison hole) or hard-fail on content (which would
    // misattribute the divergence to a code change). Only gates when at least
    // one crate in the set could actually run the guard (crates.io target with a
    // resolved version); a pure non-crates.io / unversioned set never packages
    // for comparison, so a dirty tree there is irrelevant.
    let any_guarded = sorted_names.iter().any(|name| {
        targets_crates_io(cargo_cfgs.get(name))
            && !crate_versions
                .get(name)
                .cloned()
                .unwrap_or_default()
                .is_empty()
    });
    if any_guarded {
        ensure_publish_tree_clean(ctx)?;
    }

    for (i, name) in sorted_names.iter().enumerate() {
        log.verbose(&run_per_crate_start_message(name));
        // Per-crate resolved version (own Cargo.toml `[package].version`,
        // falling back to the release version) — sourced from the plan so the
        // already-published check uses the same version the preflight queried.
        let crate_version = crate_versions.get(name).cloned().unwrap_or_default();

        let cargo_cfg = cargo_cfgs.get(name);
        let crate_cfg = all_crates.iter().find(|c| &c.name == name);

        // binstall metadata BEFORE the skip-decision packages — so
        // `local_crate_cksum` hashes the SAME on-disk tree `cargo publish`
        // uploads. The original publish wrote this table, so the crates.io
        // cksum reflects it; packaging without it would mismatch and
        // false-poison every binstall crate's clean re-cut (anodizer's own
        // `cli` crate carries `binstall.enabled: true`). Mutating in place is
        // byte-identical-by-construction: the real publish mutates the same tree
        // and never reverts it, so there is no second tree to keep in sync. The
        // tree was verified clean once before the loop, so this is the only
        // dirtiness `cargo package` will see — matching the original publish.
        if let Some(crate_cfg) = crate_cfg {
            ensure_binstall_metadata_with(ctx, crate_cfg, false, log, resolve_tag)?;
        }

        // Idempotency + poison guard: if this version already exists on
        // crates.io, the publish may be a safe re-cut (byte-identical content)
        // or a SILENT POISON (content changed but the version was not bumped —
        // `cargo publish` would skip, never shipping the new bytes, while
        // anodizer reports success and consumers get stale code). Before
        // treating an already-published version as a safe skip, prove the
        // local `.crate` is byte-identical to the published artifact by
        // comparing sha256 against the index-recorded `cksum`. The local
        // package step now reflects the binstall mutation applied above, so the
        // hash matches what the original `cargo publish` uploaded.
        //
        // The skip — and the guard — apply ONLY to crates.io targets: a custom
        // `registry =`/`index =` points cargo at a different index, so the
        // crates.io cksum describes a different (or no) artifact. For those,
        // attempt publish and let the target registry's server-side conflict
        // handling govern idempotency.
        //
        // Index check failures (network) FAIL CLOSED for an already-published
        // decision: an unreachable index cannot prove the version is absent,
        // and silently skipping a maybe-poisoned version is the bug this guard
        // exists to prevent.
        let guard = if crate_version.is_empty() || !targets_crates_io(cargo_cfg) {
            CargoSkipDecision::Publish
        } else {
            match already_published_check(name, &crate_version, &retry_policy, log) {
                Ok(None) => CargoSkipDecision::Publish,
                Ok(Some(index_cksum)) => {
                    let crate_cfg = crate_cfg.ok_or_else(|| {
                        anyhow::anyhow!(
                            "publish: '{name}-{crate_version}' is published on crates.io but its \
                             crate config is missing; cannot verify content identity"
                        )
                    })?;
                    decide_already_published(
                        name,
                        &crate_version,
                        &index_cksum,
                        crate_cfg,
                        cargo_cfg,
                        changelog_provenance_recorded(
                            ctx,
                            name,
                            &crate_version,
                            &crate_changelog_rel_path(&crate_cfg.path),
                            log,
                        ),
                        &local_cksum_check,
                        |n, v| fetch_published(n, v, &retry_policy, log),
                        log,
                    )?
                }
                Err(e) => {
                    // Fail closed: do not silently skip a version we cannot
                    // confirm is byte-identical to what shipped.
                    anyhow::bail!(
                        "publish: could not reach the crates.io index to verify '{name}-{crate_version}' \
                         is safe to skip ({e}); refusing to skip a possibly-poisoned already-published \
                         version. Resolve the network issue and re-run, or bump the version."
                    );
                }
            }
        };
        if matches!(guard, CargoSkipDecision::Skip) {
            log.status(&format!(
                "skipped '{}-{}' — already published on crates.io with verified equivalent \
                 content",
                name, crate_version
            ));
            continue;
        }

        // (binstall metadata was emitted above, before the skip-decision, so
        // the local package step the guard ran reflects the same on-disk tree
        // `cargo publish` is about to upload. It is needed regardless of the
        // skip outcome — the real release runs `--publish-only`, which skips
        // the build stage, so the table must exist before publish either way.)

        // Pre-publish gate: in multi-tag-multi-crate workspaces (e.g. cfgd)
        // per-crate tags fire independent Release.yml runs, so the upstream
        // crate's publish may not have landed on crates.io by the time this
        // downstream's publish starts. The wait_for_workspace_deps block,
        // when enabled, polls crates.io for every workspace-internal dep at
        // its pinned version and blocks until each appears. Disabled by
        // default — anodize's own workspace publishes lockstep within one
        // Release.yml run, where in-loop topological order + the post-
        // publish poll_crates_io_index call below already cover the race.
        let wait_cfg = cargo_cfg
            .and_then(|c| c.wait_for_workspace_deps.as_ref())
            .cloned()
            .unwrap_or_default();
        if wait_cfg.resolved_enabled() {
            let crate_path = crate_paths
                .get(name)
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            let manifest_path = std::path::Path::new(&crate_path).join("Cargo.toml");
            // Workspace-internal dep set: every crate in the same anodize
            // config (top-level + workspaces overlay). External crates.io
            // deps (serde, tokio, ...) get filtered out by the name check.
            let workspace_names: HashSet<&str> =
                all_crates.iter().map(|c| c.name.as_str()).collect();
            let deps =
                workspace_deps_for_crate(&manifest_path, &workspace_names, &mut ws_root_cache);
            if deps.is_empty() {
                log.verbose(&format!(
                    "'{name}' has no workspace-internal deps with \
                     a literal version pin — gate is a no-op"
                ));
            } else {
                wait_for_workspace_deps_to_appear(name, &deps, &wait_cfg, log)
                    .with_context(|| format!("publish: wait_for_workspace_deps for '{name}'"))?;
            }
        }

        let cmd = publish_command(name, cargo_cfg);
        log.verbose(&format!("running {}", cmd.join(" ")));

        // Defense in depth: even though poll_crates_io_index already waits
        // for the prior crate to land on the index edge anodizer queries,
        // cargo's own resolution may hit a stale Fastly edge a beat later.
        // run_cargo_publish_with_retry narrows retry exclusively to the
        // sparse-index propagation failure signatures so real errors still
        // fast-fail.
        run_cargo_publish_with_retry(
            &cmd,
            &format!("cargo publish -p {}", name),
            log,
            PUBLISH_PROPAGATION_BACKOFF,
            registry_token,
        )?;

        log.status(&format!("published crate '{}'", name));

        // Record the published identity NOW, at the instant of success, so
        // a later crate's failure can still drive rollback to yank this
        // one. Registry/index come from the same `publish.cargo` block the
        // publish used, so the yank targets the matching registry. The
        // version is the per-crate resolved version (workspaces with mixed
        // cadences publish different versions per crate).
        //
        // When the per-crate manifest was unreadable, crate_version is empty
        // (the skip-decision treats it as "not yet published" to avoid a
        // false-skip). For the yank record we fall back to the global release
        // version so rollback can still attempt a yank. If even that is
        // empty, warn: `cargo yank --version ""` is rejected and a silent
        // under-yank is worse than an explicit manual-cleanup message.
        let yank_version = if !crate_version.is_empty() {
            crate_version.clone()
        } else {
            ctx.version()
        };
        if yank_version.is_empty() {
            log.warn(&format!(
                "cargo published '{name}' with no resolvable version; it CANNOT be \
                 auto-yanked on rollback — verify and `cargo yank` it manually if a \
                 later crate fails this run"
            ));
        } else {
            record.push(CargoYankTarget {
                name: name.clone(),
                version: yank_version,
                registry: cargo_cfg.and_then(|c| c.registry.clone()),
                index: cargo_cfg.and_then(|c| c.index.clone()),
            });
        }

        // If there are later crates that depend on this one, wait for the index.
        let has_dependents = sorted_names[i + 1..].iter().any(|later| {
            deps_map
                .get(later)
                .map(|d| d.contains(name))
                .unwrap_or(false)
        });

        if has_dependents && !crate_version.is_empty() {
            let timeout = cargo_cfg
                .and_then(|c| c.index_timeout)
                .unwrap_or(DEFAULT_INDEX_TIMEOUT_SECS);
            if timeout == 0 {
                log.warn(&format!(
                    "skipped index poll for '{}' — index_timeout is 0 (dependents may fail)",
                    name
                ));
            } else {
                log.verbose(&format!(
                    "waiting for {}-{} in crates.io index (timeout={}s)…",
                    name, crate_version, timeout
                ));
                poll_crates_io_index(name, &crate_version, timeout, log)
                    .with_context(|| format!("publish: index poll for '{}'", name))?;
            }
        }
    }

    Ok(())
}

pub(crate) struct PublishedCrateRef {
    pub(crate) name: String,
    pub(crate) version: String,
}

/// Returns the canonical published crate for `primary_ref` reporting.
///
/// Multi-crate workspaces release many crates in one run; the
/// [`PublishEvidence`](anodizer_core::PublishEvidence) schema's
/// `primary_ref` carries one canonical URL. We prefer the crate whose
/// `name` matches `ctx.config.project_name` so operators see the marquee
/// crate (e.g. `anodizer` from the `anodizer-*` workspace) instead of
/// whichever crate happens to iterate first. If no such match exists
/// (project_name unset, or no eligible crate matches it), fall back to
/// the first crate with `publish.cargo` configured.
pub(crate) fn first_published_crate(ctx: &Context) -> Option<PublishedCrateRef> {
    let eligible = |c: &&CrateConfig| c.publish.as_ref().and_then(|p| p.cargo.as_ref()).is_some();
    let project_name = ctx.config.project_name.as_str();
    let universe = ctx.config.crate_universe();
    let name = universe
        .iter()
        .copied()
        .find(|c| !project_name.is_empty() && c.name == project_name && eligible(c))
        .or_else(|| universe.iter().copied().find(eligible))
        .map(|c| c.name.clone())?;
    let version = {
        let tag = ctx
            .git_info
            .as_ref()
            .map(|g| g.tag.clone())
            .unwrap_or_else(|| ctx.version());
        tag.strip_prefix('v').unwrap_or(&tag).to_string()
    };
    if version.is_empty() {
        return None;
    }
    Some(PublishedCrateRef { name, version })
}
