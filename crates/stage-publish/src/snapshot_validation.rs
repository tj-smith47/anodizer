//! Snapshot / dry-run emission validation.
//!
//! In a real release the anodize-only emission features — binstall
//! `[package.metadata.binstall]`, the Nix flake + per-crate derivations, and
//! version-sync — mutate source files or push to a remote. Snapshot and
//! dry-run modes skip those side effects, which historically meant a BROKEN
//! emission (a binstall `pkg_url` pointing at an asset the release never
//! produces, a nix `packages.<system>` mapped to a missing asset, a crate
//! with no resolvable version) passed every local check and only blew up at
//! `cargo binstall` / `nix build` time on a consumer's machine.
//!
//! This module closes that blindspot: in snapshot/dry-run it RENDERS the
//! would-be output in-memory — never mutating source, never cloning a repo,
//! never pushing — and cross-checks the rendered emission against the asset
//! set the run actually produced (`ctx.artifacts`). A mismatch fails the
//! snapshot loud, naming the crate + emission + what is wrong.
//!
//! Runs per in-scope crate with that crate's own version/name/tag scope
//! (via [`anodizer_core::crate_scope::with_crate_scope`]), so it is correct in
//! all four config modes: single-crate, workspace-lockstep, per-crate, and
//! `--all`.

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{BinstallConfig, CrateConfig};
use anodizer_core::context::Context;
use anodizer_core::crate_scope::{resolve_crate_tag, with_crate_scope};
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use crate::nix;
use crate::util;

/// Entry point: validate every in-scope crate's snapshot emissions.
///
/// No-op outside snapshot/dry-run. Iterates the crate universe honoring
/// `--crate` selection, and for each crate carrying a binstall / nix /
/// version-sync emission, re-scopes the template vars to that crate and runs
/// the matching cross-check. The first broken emission aborts with an
/// actionable error.
pub(crate) fn validate_snapshot_emissions(ctx: &mut Context, log: &StageLogger) -> Result<()> {
    // A target-restricted build (`--targets=`, used by the sharded determinism
    // harness so each runner only validates the targets it can natively build)
    // intentionally produces a SUBSET of the configured targets' artifacts.
    // Cross-platform publishers (homebrew, nix, scoop, …) aggregate assets
    // across every target, so their emissions cannot be cross-checked against a
    // by-construction-incomplete asset set: nix finds no Linux/Darwin archive, a
    // homebrew formula is missing platforms, etc. Step aside rather than fail on
    // the partial set — exactly as the harness skips produce-stages that don't
    // belong to a shard. A full (non-sharded) `--snapshot` / `--dry-run` still
    // exercises emission-validate end-to-end.
    if ctx.options.partial_target.is_some() {
        log.status(
            "emission-validate: skipped — build is target-restricted (--targets); \
             cross-platform emission checks require the full artifact set",
        );
        return Ok(());
    }
    validate_snapshot_emissions_with_resolver(ctx, log, &resolve_crate_tag_or_snapshot)
}

/// Per-crate tag source for the emission-validate pass.
///
/// A real release (or its dry-run) tags every selected crate, so this defers to
/// [`resolve_crate_tag`]. In SNAPSHOT mode there are no tags by design — the
/// build stamps every crate with the global synthesized snapshot version
/// (`<base>-SNAPSHOT-<sha>`, set on `Version` by `apply_snapshot_template_vars`)
/// — so a tagless crate falls back to that version, the one the produced
/// artifacts actually carry. Without this, a snapshot run of a binstall/nix/
/// version-sync crate aborts the whole pipeline at `with_crate_scope`'s
/// fail-loud tag guard (the determinism harness, which only ever builds an
/// untagged HEAD in snapshot, can never complete a run otherwise).
fn resolve_crate_tag_or_snapshot(ctx: &Context, crate_cfg: &CrateConfig) -> Option<String> {
    resolve_crate_tag(ctx, crate_cfg).or_else(|| snapshot_version_fallback(ctx))
}

/// The global snapshot version to scope a tagless crate to, or `None` outside
/// snapshot mode (a real release must resolve a real tag — never papered over).
fn snapshot_version_fallback(ctx: &Context) -> Option<String> {
    if !ctx.is_snapshot() {
        return None;
    }
    let version = ctx.version();
    (!version.trim().is_empty()).then_some(version)
}

/// Inner body of [`validate_snapshot_emissions`] with the per-crate tag source
/// injected. Production passes [`resolve_crate_tag`] (git-backed); tests pass a
/// closure returning fixed tags so the version-dimension fix can be exercised
/// without a git fixture.
fn validate_snapshot_emissions_with_resolver(
    ctx: &mut Context,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    if !ctx.is_snapshot() && !ctx.is_dry_run() {
        return Ok(());
    }

    // The version that actually NAMED the produced archives. In snapshot it is
    // the synthesized `<base>-SNAPSHOT-<sha>` (set on the global `Version` var
    // by `apply_snapshot_template_vars` before the pipeline ran); in real-
    // release dry-run it is the real version. The asset cross-check MUST render
    // binstall `pkg_url` / nix derivation URLs with THIS version so a correct
    // `{{ .Version }}` resolves to the same stem the archives carry — not a
    // tag-re-derived version that would never match the `-SNAPSHOT-<sha>` asset.
    let artifact_version = ctx.version();

    let crates = in_scope_crates(ctx);
    for crate_cfg in &crates {
        let has_binstall = crate_cfg
            .binstall
            .as_ref()
            .is_some_and(|b| b.enabled.unwrap_or(false));
        let has_version_sync = crate_cfg
            .version_sync
            .as_ref()
            .is_some_and(|v| v.enabled.unwrap_or(false));
        let has_nix = crate_cfg.publish.as_ref().is_some_and(|p| p.nix.is_some());
        if !has_binstall && !has_version_sync && !has_nix {
            continue;
        }

        with_crate_scope(ctx, crate_cfg, resolve_tag, |ctx| {
            // version-sync: the per-crate scope already fail-loud-resolved a
            // parseable version (with_crate_scope errors otherwise), so this
            // is the snapshot twin of the real-release guard — a crate with
            // no tag / an unparseable tag is caught here, not at release time.
            // It runs FIRST, while `Version` is the per-crate tag-derived value,
            // because that is the version a real release would stamp.
            if has_version_sync {
                validate_version_sync(ctx, crate_cfg)?;
            }

            // For the ASSET cross-check, swap `Version`/`RawVersion` to the
            // version that named the produced archives (keeping the per-crate
            // `ProjectName`/`Name`/`Tag` scope intact for multi-crate configs).
            // A `{{ .Version }}` in a correct pkg_url now renders the same stem
            // the archives carry, killing the snapshot false-positive.
            let restore_version = scope_artifact_version(ctx, &artifact_version);
            let asset_check = (|| -> Result<()> {
                if has_binstall && let Some(bs) = crate_cfg.binstall.as_ref() {
                    validate_binstall(ctx, crate_cfg, bs, log)?;
                }
                if has_nix {
                    validate_nix(ctx, crate_cfg, log)?;
                }
                Ok(())
            })();
            restore_artifact_version(ctx, restore_version);
            asset_check
        })?;
    }

    // Whole-artifact schema conformance: render each configured publisher's
    // manifest and validate it against the registry's vendored schema. Catches
    // structural defects the per-field asset cross-checks above cannot — a
    // manifest that omits a registry-required key or carries a wrong-typed value
    // would be rejected at submission, only after a real release uploaded it.
    //
    // Each validator scopes its OWN per-crate render to that crate's version/
    // name/tag (via `with_crate_scope`, the same resolver the cross-checks use)
    // so a per-crate manifest is validated against the version a real release
    // would stamp — matching what the live publish path now renders. Cross-crate
    // aggregation (the nix root flake) stays under the global scope since it is
    // version-independent. The resolver is threaded through so tests can drive
    // the version dimension without a git fixture.
    crate::schema_validation::validate_publisher_schemas(ctx, log, resolve_tag)?;

    Ok(())
}

/// Override `Version`/`RawVersion` to the artifact-naming version, returning
/// the prior `(Version, RawVersion)` so [`restore_artifact_version`] can undo
/// the swap. A blank `version` is a no-op (no archives to cross-check against).
fn scope_artifact_version(ctx: &mut Context, version: &str) -> (Option<String>, Option<String>) {
    if version.trim().is_empty() {
        return (None, None);
    }
    let prior = (
        ctx.template_vars().get("Version").cloned(),
        ctx.template_vars().get("RawVersion").cloned(),
    );
    ctx.template_vars_mut().set("Version", version);
    ctx.template_vars_mut().set("RawVersion", version);
    prior
}

/// Restore `Version`/`RawVersion` captured by [`scope_artifact_version`].
fn restore_artifact_version(ctx: &mut Context, prior: (Option<String>, Option<String>)) {
    let (version, raw) = prior;
    match version {
        Some(v) => ctx.template_vars_mut().set("Version", &v),
        None => {
            ctx.template_vars_mut().unset("Version");
        }
    }
    match raw {
        Some(v) => ctx.template_vars_mut().set("RawVersion", &v),
        None => {
            ctx.template_vars_mut().unset("RawVersion");
        }
    }
}

/// The crate universe the validator walks: every top-level + workspace crate,
/// filtered to the `--crate` selection when one is present. Mirrors the build
/// stage's selection semantics so the validated set equals the released set in
/// all four config modes.
fn in_scope_crates(ctx: &Context) -> Vec<CrateConfig> {
    let selected = &ctx.options.selected_crates;
    util::all_crates(ctx)
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .collect()
}

// ---------------------------------------------------------------------------
// version-sync
// ---------------------------------------------------------------------------

/// Assert the in-scope crate resolved a valid, non-empty, parseable per-crate
/// version. The `with_crate_scope` wrapper already errors on a no-tag /
/// unparseable-tag crate; this is the belt-and-suspenders check that the
/// scoped `Version` var is actually populated and parseable, matching the
/// real-release fail-loud guard.
fn validate_version_sync(ctx: &Context, crate_cfg: &CrateConfig) -> Result<()> {
    let version = ctx
        .template_vars()
        .get("RawVersion")
        .or_else(|| ctx.template_vars().get("Version"))
        .cloned()
        .unwrap_or_default();
    if version.trim().is_empty() {
        bail!(
            "version-sync: crate '{}' resolved an empty version in snapshot \
             validation; a release would stamp Cargo.toml with no version",
            crate_cfg.name
        );
    }
    anodizer_core::git::parse_semver_tag(&version).with_context(|| {
        format!(
            "version-sync: crate '{}' resolved version '{}' which is not parseable \
             semver; cargo-binstall and the cargo publish would both reject it",
            crate_cfg.name, version
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// binstall
// ---------------------------------------------------------------------------

/// Render the crate's `[package.metadata.binstall]` table in-memory and
/// cross-check every resolved `pkg_url` (top-level and per-override) against
/// the assets the run actually produced for this crate. A `pkg_url` whose
/// asset filename matches no produced archive is the exact 404 class —
/// `cargo binstall` would request a URL the release never uploaded.
fn validate_binstall(
    ctx: &Context,
    crate_cfg: &CrateConfig,
    bs: &BinstallConfig,
    log: &StageLogger,
) -> Result<()> {
    let produced = produced_archives(ctx, &crate_cfg.name);
    if produced.is_empty() {
        // No archives for this crate in this (possibly sharded) run — nothing
        // to cross-check against. The binstall render itself is still
        // exercised below so a template error is caught.
        log.verbose(&format!(
            "binstall: crate '{}' produced no archives in this snapshot shard; \
             rendering pkg_url without an asset cross-check",
            crate_cfg.name
        ));
    }

    // Top-level pkg_url. cargo-binstall substitutes its own `{ target }` /
    // `{ arch }` / `{ … }` tokens per platform, so a TOKENED url resolves to a
    // different asset on each target — at least one produced asset must match.
    // A url with NO cargo-binstall token, though, is the SAME literal on every
    // platform: it can only ever fetch one stem, so it would 404 on every other
    // produced target. Require it to match ALL produced targets, not just one,
    // so a hardcoded single-platform stem (`…-linux-amd64.tar.gz`) is rejected.
    // The crate/package name and resolved artifact version cargo-binstall's
    // `{ name }` / `{ version }` tokens resolve to; substituting them lets a
    // fully-tokened `pkg_url` match exactly rather than falling through to the
    // looser literal-segment check.
    let name = crate_cfg.name.as_str();
    let version = ctx.version();

    if let Some(ref pkg_url) = bs.pkg_url {
        let rendered = ctx
            .render_template(pkg_url)
            .with_context(|| format!("binstall: render pkg_url for crate '{}'", crate_cfg.name))?;
        let asset = asset_filename(&rendered);
        if !produced.is_empty() {
            let tokened = asset.contains('{');
            let matched: Vec<&ProducedAsset> = produced
                .iter()
                .filter(|p| binstall_asset_matches(&asset, &p.name, &p.target, name, &version))
                .collect();
            let ok = if tokened {
                !matched.is_empty()
            } else {
                // Untokened: must resolve correctly for every produced target.
                matched.len() == produced.len()
            };
            if !ok {
                bail!(binstall_mismatch_msg(
                    &crate_cfg.name,
                    "pkg_url",
                    &asset,
                    &produced,
                ));
            }
        }
    }

    // Per-override pkg_url: each is target-specific (key = triple), so it must
    // match a produced asset for THAT triple specifically.
    if let Some(ref overrides) = bs.overrides {
        // The full set of triples this crate is configured to build. A
        // `--single-target` / sharded snapshot produces archives for only a
        // subset of them, so "this run produced no asset for triple T" alone
        // does NOT prove the override is bogus — only an override pointing at a
        // triple OUTSIDE the configured set is a genuine misconfiguration.
        let configured = configured_target_set(ctx, crate_cfg);
        for (triple, ovr) in overrides {
            let Some(ref ovr_url) = ovr.pkg_url else {
                continue;
            };
            let rendered = ctx.render_template(ovr_url).with_context(|| {
                format!(
                    "binstall: render overrides.{triple} pkg_url for crate '{}'",
                    crate_cfg.name
                )
            })?;
            let asset = asset_filename(&rendered);
            let for_triple: Vec<&ProducedAsset> = produced
                .iter()
                .filter(|p| p.target.as_deref() == Some(triple.as_str()))
                .collect();
            if for_triple.is_empty() {
                // The run produced no asset for this triple. Bail only when the
                // triple is not in the crate's configured target set (a real
                // "override points at a target the release never builds" 404).
                // When the triple IS configured but just wasn't built in this
                // shard, there is nothing to cross-check — skip it.
                if !produced.is_empty() && !configured.contains(triple.as_str()) {
                    bail!(
                        "binstall: crate '{}' override '{}' targets a triple the release \
                         never builds; cargo binstall on '{}' would 404. \
                         Configured targets: {}",
                        crate_cfg.name,
                        triple,
                        triple,
                        configured_targets_str(&configured),
                    );
                }
                log.verbose(&format!(
                    "binstall: crate '{}' override '{}' not built in this snapshot shard; \
                     skipping its asset cross-check",
                    crate_cfg.name, triple
                ));
                continue;
            }
            if !for_triple
                .iter()
                .any(|p| binstall_asset_matches(&asset, &p.name, &p.target, name, &version))
            {
                bail!(
                    "binstall: crate '{}' override '{}' pkg_url resolves to asset '{}' \
                     which is not among the archives produced for that triple ({}); \
                     cargo binstall would 404. Fix overrides.{}.pkg_url to match a \
                     produced asset name.",
                    crate_cfg.name,
                    triple,
                    asset,
                    for_triple
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    triple,
                );
            }
        }
    }
    Ok(())
}

/// A produced archive asset: its filename and the target triple it was built
/// for (when known).
struct ProducedAsset {
    name: String,
    target: Option<String>,
}

/// Gather the archive assets this run produced for `crate_name`, sorted by
/// name for deterministic error output.
fn produced_archives(ctx: &Context, crate_name: &str) -> Vec<ProducedAsset> {
    let mut out: Vec<ProducedAsset> = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .map(|a| ProducedAsset {
            name: a.name.clone(),
            target: a.target.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The set of target triples `crate_cfg` is configured to build: the union of
/// each build's `targets:` (its own list when set, else the global
/// `defaults.targets`). Used to tell a genuinely bogus override (a triple the
/// release never builds) from one merely not built in the current snapshot shard.
fn configured_target_set(
    ctx: &Context,
    crate_cfg: &CrateConfig,
) -> std::collections::BTreeSet<String> {
    let default_targets: Vec<String> = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.targets.clone())
        .unwrap_or_default();
    let mut set = std::collections::BTreeSet::new();
    match crate_cfg.builds.as_deref() {
        Some(builds) if !builds.is_empty() => {
            for build in builds {
                match build.targets.as_deref() {
                    Some(ts) => set.extend(ts.iter().cloned()),
                    None => set.extend(default_targets.iter().cloned()),
                }
            }
        }
        _ => set.extend(default_targets),
    }
    set
}

fn configured_targets_str(set: &std::collections::BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// The final path segment of a (possibly templated) URL — the asset filename.
fn asset_filename(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// Decide whether a binstall-resolved asset filename matches a produced
/// archive. cargo-binstall's own `{ target }` / `{ version }` / `{ name }`
/// tokens (which anodize deliberately leaves intact) are substituted with the
/// produced asset's facts before comparison; the load-bearing `{ target }` is
/// the field that distinguishes the 404 class (a `pkg_url` baking
/// `linux-amd64` while the release produces `x86_64-unknown-linux-gnu`).
///
/// `{ name }` (the crate/package name) and `{ version }` (the resolved
/// artifact version) are substituted from the values anodize already knows, so
/// an exact filename match fires for a fully-tokened `pkg_url` and the loose
/// literal-segment fallback is reached less often. `{ target-arch }` (and the
/// soft `{ arch }` alias) is deliberately NOT substituted: cargo-binstall
/// derives it from `target_lexicon::Architecture` with its own mapping, so a
/// hand-rolled substitution would risk the exact false mismatch this check
/// guards — it stays on the literal-segment fallback below.
///
/// When all substituted tokens resolve, an exact filename match is required.
/// When an unmodeled token survives, fall back to requiring every literal
/// (non-token) segment of the candidate to appear in the produced name — this
/// still catches a wrong asset stem while tolerating tokens anodize does not
/// enumerate.
fn binstall_asset_matches(
    candidate: &str,
    produced_name: &str,
    produced_target: &Option<String>,
    name: &str,
    version: &str,
) -> bool {
    let mut resolved = candidate.to_string();
    if let Some(t) = produced_target {
        resolved = substitute_binstall_token(&resolved, "target", t);
    }
    if !name.is_empty() {
        resolved = substitute_binstall_token(&resolved, "name", name);
    }
    if !version.is_empty() {
        resolved = substitute_binstall_token(&resolved, "version", version);
    }
    if resolved == produced_name {
        return true;
    }
    // Tolerate residual cargo-binstall tokens by comparing literal segments:
    // every non-token chunk of the candidate must be a substring of the
    // produced name. A mismatched stem (the 404 class) fails this because the
    // literal `linux-amd64` chunk is absent from `…x86_64-unknown-linux-gnu…`.
    if !resolved.contains('{') {
        return false;
    }
    literal_segments(&resolved).all(|seg| seg.is_empty() || produced_name.contains(seg))
}

/// Replace a cargo-binstall `{ <token> }` placeholder (tolerant of internal
/// whitespace, e.g. `{ target }` or `{target}`) with `value`.
fn substitute_binstall_token(s: &str, token: &str, value: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let close = open + close_rel;
        let inner = rest[open + 1..close].trim();
        if inner == token {
            out.push_str(value);
        } else {
            out.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Yield the literal (outside-`{...}`) segments of a templated string.
fn literal_segments(s: &str) -> impl Iterator<Item = &str> {
    let mut segments: Vec<&str> = Vec::new();
    let mut rest = s;
    loop {
        match rest.find('{') {
            Some(open) => {
                segments.push(&rest[..open]);
                match rest[open..].find('}') {
                    Some(close_rel) => rest = &rest[open + close_rel + 1..],
                    None => break,
                }
            }
            None => {
                segments.push(rest);
                break;
            }
        }
    }
    segments.into_iter()
}

fn binstall_mismatch_msg(
    crate_name: &str,
    field: &str,
    asset: &str,
    produced: &[ProducedAsset],
) -> String {
    let names: Vec<&str> = produced.iter().map(|p| p.name.as_str()).collect();
    format!(
        "binstall: crate '{}' {} resolves to asset '{}' which matches none of the \
         archives this release produces ({}); cargo binstall would request a URL the \
         release never uploaded (404). Fix binstall.{} to reference a produced asset name.",
        crate_name,
        field,
        asset,
        names.join(", "),
        field,
    )
}

// ---------------------------------------------------------------------------
// nix
// ---------------------------------------------------------------------------

/// Render the crate's nix derivation + the merged root flake in-memory and
/// assert: (a) the flake is well-formed (braces balance + overlay lines
/// round-trip the recovery parser); and (b) every `packages.<system>` the
/// flake exposes is backed — the derivation's `urlMap` maps that nix-system
/// double to an asset the run actually produced. A system mapped to a missing
/// asset fails: `nix build .#<name>` on that system would fetch a 404 URL.
fn validate_nix(ctx: &mut Context, crate_cfg: &CrateConfig, log: &StageLogger) -> Result<()> {
    // The validation twin must tolerate the dry-run / sharded zero-archive case
    // the real publish never sees: `render_nix_for_validation` bails when the
    // crate produced no Linux/Darwin archives (correct for publish — you cannot
    // publish a derivation with no binaries), so guard the render here. With no
    // produced assets there is nothing to cross-check; skip exactly as binstall.
    let produced = produced_archives(ctx, &crate_cfg.name);
    if produced.is_empty() {
        log.verbose(&format!(
            "nix: crate '{}' produced no archives in this snapshot shard; \
             skipping nix emission validation (no assets to cross-check)",
            crate_cfg.name,
        ));
        return Ok(());
    }

    let Some(render) = nix::render_nix_for_validation(ctx, &crate_cfg.name, log)? else {
        // Publisher would skip (skip / if-falsy / skip_upload); nothing to
        // validate.
        return Ok(());
    };

    // The derivation expression itself must be structurally well-formed —
    // `nix-build` rejects an unbalanced expression outright. Same
    // string/comment-aware delimiter balance the flake gets, so a literal
    // brace inside the `installPhase = ''…''` body or a `meta.description`
    // string does not miscount.
    nix::nix_delimiters_balanced(&render.expr).with_context(|| {
        format!(
            "nix: crate '{}' derivation expression has unbalanced delimiters; \
             nix-build would reject it",
            crate_cfg.name,
        )
    })?;

    // The flake the next publish would write merges this package into the
    // prior set; for validation we render the single-package flake and assert
    // it is well-formed via the same recovery parser the publish loop trusts.
    let pkg = nix::FlakePackage {
        attr: render.name.clone(),
        path: format!("pkgs/{}/default.nix", render.name),
    };
    let flake = nix::generate_flake(&[pkg])?;
    let recovered = nix::flake_is_well_formed(&flake).with_context(|| {
        format!(
            "nix: crate '{}' generated a malformed flake.nix",
            crate_cfg.name
        )
    })?;
    if recovered.len() != 1 || recovered[0].attr != render.name {
        bail!(
            "nix: crate '{}' flake overlay did not round-trip its package attr '{}'",
            crate_cfg.name,
            render.name,
        );
    }

    // System -> asset cross-check. The flake exposes `packages.<system>` for
    // every FLAKE_SYSTEMS double; the derivation's urlMap (render.archives)
    // resolves each system to a release asset. Every system the derivation
    // maps MUST point at an asset the run produced. `produced` is guaranteed
    // non-empty here — the empty case returned early above.
    let produced_assets: std::collections::BTreeSet<&str> =
        produced.iter().map(|p| p.name.as_str()).collect();

    for (system, url, _hash) in &render.archives {
        let asset = asset_filename(url);
        if !produced_assets.contains(asset.as_str()) {
            bail!(
                "nix: crate '{}' derivation maps system '{}' to asset '{}' which the \
                 release does not produce ({}); `nix build .#{}` on '{}' would fetch a \
                 404 URL. Fix the nix url_template / archive name to match a produced asset.",
                crate_cfg.name,
                system,
                asset,
                produced
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                render.name,
                system,
            );
        }
    }

    // Sanity: the systems the derivation maps must be a subset of the doubles
    // the flake advertises, otherwise `nix build .#<name>` resolves a system
    // the derivation cannot satisfy.
    for (system, _, _) in &render.archives {
        if !nix::FLAKE_SYSTEMS.contains(&system.as_str()) {
            bail!(
                "nix: crate '{}' derivation maps non-standard system '{}' not advertised \
                 by the flake; the flake exposes only {:?}",
                crate_cfg.name,
                system,
                nix::FLAKE_SYSTEMS,
            );
        }
    }

    log.verbose(&format!(
        "nix: crate '{}' snapshot emission validated ({} system(s) cross-checked)",
        crate_cfg.name,
        render.archives.len()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        BinstallConfig, BinstallOverride, BuildConfig, CrateConfig, NixConfig, PublishConfig,
        RepositoryConfig, VersionSyncConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;

    fn log() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    /// Seed a snapshot context whose `Version`/`ProjectName` are pre-scoped
    /// (the per-crate scope `with_crate_scope` would otherwise apply) so the
    /// individual `validate_*` functions can be exercised without a git fixture.
    fn scoped_ctx(crate_cfg: CrateConfig) -> Context {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![crate_cfg])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "cfgd");
        ctx.template_vars_mut().set("Name", "cfgd");
        ctx
    }

    /// Add an archive artifact for `crate_name`/`target` with the canonical
    /// asset `name` plus url+sha256 metadata so the nix render path accepts it.
    fn add_archive(ctx: &mut Context, crate_name: &str, target: &str, name: &str) {
        let mut metadata = HashMap::new();
        metadata.insert("url".to_string(), format!("https://example.com/{name}"));
        metadata.insert("sha256".to_string(), "a".repeat(64));
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{name}")),
            name: name.to_string(),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata,
            size: None,
        });
    }

    fn binstall_crate(binstall: BinstallConfig) -> CrateConfig {
        CrateConfig {
            name: "cfgd".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            binstall: Some(binstall),
            ..Default::default()
        }
    }

    // -- binstall -----------------------------------------------------------

    /// The 404 class: the produced archive uses the Rust target triple in its
    /// name (`cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz`) but the binstall
    /// `pkg_url` bakes a `linux-amd64` stem the release never
    /// produces. Snapshot validation MUST fail.
    #[test]
    fn binstall_pkg_url_pointing_at_missing_asset_fails() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        let err = validate_binstall(&ctx, &cfg, &bs, &log()).expect_err("must catch the 404");
        let msg = format!("{err}");
        assert!(msg.contains("cfgd"), "names the crate: {msg}");
        assert!(
            msg.contains("cfgd-1.0.0-linux-amd64.tar.gz"),
            "names the missing asset: {msg}"
        );
        assert!(msg.contains("404"), "explains the failure class: {msg}");
    }

    /// Same produced archive, but a correct `pkg_url` whose asset name (after
    /// substituting cargo-binstall's `{ target }`) equals the produced asset.
    /// Must pass.
    #[test]
    fn binstall_pkg_url_matching_produced_asset_passes() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-{ target }.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        validate_binstall(&ctx, &cfg, &bs, &log()).expect("correct pkg_url passes");
    }

    /// `snapshot_version_fallback` yields the global snapshot version ONLY in
    /// snapshot mode; a real release returns `None` so the fail-loud tag guard
    /// still protects it.
    #[test]
    fn snapshot_version_fallback_is_snapshot_only() {
        let cfg = binstall_crate(BinstallConfig::default());

        let snap = scoped_ctx(cfg.clone());
        assert_eq!(snapshot_version_fallback(&snap).as_deref(), Some("1.0.0"));

        let mut real = TestContextBuilder::new().crates(vec![cfg]).build();
        real.template_vars_mut().set("Version", "1.0.0");
        assert!(
            snapshot_version_fallback(&real).is_none(),
            "a real release must resolve a real tag, never fall back"
        );
    }

    /// Regression (determinism harness): a binstall crate whose HEAD carries no
    /// matching release tag — the only state the harness ever builds in snapshot
    /// — must scope to the global snapshot version, not abort the whole run at
    /// `with_crate_scope`'s fail-loud tag guard. The bare no-tag resolver
    /// reproduces the harness failure; the snapshot fallback recovers it.
    #[test]
    fn snapshot_no_tag_crate_recovers_via_snapshot_fallback() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-{ target }.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let make = || {
            let mut ctx = scoped_ctx(cfg.clone());
            add_archive(
                &mut ctx,
                "cfgd",
                "x86_64-unknown-linux-gnu",
                "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
            );
            ctx
        };

        let mut ctx = make();
        let err = validate_snapshot_emissions_with_resolver(&mut ctx, &log(), &|_, _| None)
            .expect_err("a tagless crate with no fallback must fail loud");
        assert!(
            format!("{err}").contains("release tag"),
            "the failure is the missing per-crate tag guard: {err}"
        );

        let mut ctx = make();
        validate_snapshot_emissions_with_resolver(&mut ctx, &log(), &|c, _| {
            snapshot_version_fallback(c)
        })
        .expect(
            "snapshot fallback lets a tagless crate validate against its snapshot-version assets",
        );
    }

    /// A top-level `pkg_url` with NO cargo-binstall `{ target }` token hardcodes
    /// a single platform stem. It matches the linux asset but would 404 on the
    /// darwin asset the same release produces — so it must FAIL even though one
    /// produced target matches.
    #[test]
    fn binstall_untokened_pkg_url_hardcoding_one_platform_fails() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            // No `{ target }` — the same `…linux-amd64…` asset for every target.
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        // Produced for TWO targets; the url can only ever fetch the linux one.
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-linux-amd64.tar.gz",
        );
        add_archive(
            &mut ctx,
            "cfgd",
            "aarch64-apple-darwin",
            "cfgd-1.0.0-darwin-arm64.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        let err = validate_binstall(&ctx, &cfg, &bs, &log()).expect_err(
            "untokened single-platform pkg_url must fail against a multi-target release",
        );
        assert!(format!("{err}").contains("404"), "{err}");
    }

    /// An untokened `pkg_url` is fine when the release produces exactly ONE
    /// archive that it matches (single-target build): nothing to 404 on.
    #[test]
    fn binstall_untokened_pkg_url_single_target_passes() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-linux-amd64.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        validate_binstall(&ctx, &cfg, &bs, &log())
            .expect("untokened url matching the only produced asset passes");
    }

    /// Mutation-style proof: with the cross-check disabled, the broken
    /// `pkg_url` would be accepted. `binstall_asset_matches` IS the check —
    /// if it always returned true, the 404 config would pass. This pins that
    /// the matcher actually rejects the mismatched stem.
    #[test]
    fn binstall_matcher_rejects_mismatched_stem() {
        // The exact comparison the cross-check performs for the 404 case.
        assert!(
            !binstall_asset_matches(
                "cfgd-1.0.0-linux-amd64.tar.gz",
                "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &Some("x86_64-unknown-linux-gnu".to_string()),
                "cfgd",
                "1.0.0",
            ),
            "matcher must reject a GoReleaser-style stem against a triple-named asset"
        );
        // And accepts the templated-correct case.
        assert!(
            binstall_asset_matches(
                "cfgd-1.0.0-{ target }.tar.gz",
                "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &Some("x86_64-unknown-linux-gnu".to_string()),
                "cfgd",
                "1.0.0",
            ),
            "matcher must accept the {{ target }}-templated asset"
        );
    }

    /// Token-substitution stress: a fully cargo-binstall-tokened stem
    /// (`{ name }-{ version }-{ target }`) is resolved to an EXACT filename
    /// match from the name/version/triple anodize knows — not left to the loose
    /// literal-segment fallback. The same stem must REJECT a wrong-version and a
    /// wrong-name produced asset.
    #[test]
    fn binstall_matcher_substitutes_name_and_version_tokens() {
        let candidate = "{ name }-{ version }-{ target }.tar.gz";
        let target = Some("x86_64-unknown-linux-gnu".to_string());
        // Exact: name/version/target all resolve, equalling the produced name.
        assert!(
            binstall_asset_matches(
                candidate,
                "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &target,
                "cfgd",
                "1.0.0",
            ),
            "a fully-tokened stem must match the produced asset exactly"
        );
        // Wrong version: the produced asset carries 1.0.1, the resolved name is
        // 1.0.0 — no token survives, so the exact compare fails (no fallback).
        assert!(
            !binstall_asset_matches(
                candidate,
                "cfgd-1.0.1-x86_64-unknown-linux-gnu.tar.gz",
                &target,
                "cfgd",
                "1.0.0",
            ),
            "a wrong-version produced asset must not match once {{ version }} is substituted"
        );
        // Wrong name: produced asset is for a different crate.
        assert!(
            !binstall_asset_matches(
                candidate,
                "other-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &target,
                "cfgd",
                "1.0.0",
            ),
            "a wrong-name produced asset must not match once {{ name }} is substituted"
        );
    }

    /// False-MATCH guard: a candidate whose literal segments are a subset of a
    /// WRONG asset must not sneak through the loose fallback once `{ name }` and
    /// `{ version }` are substituted into concrete literals. Here the candidate
    /// names crate `cfgd`@`1.0.0`, but the only produced asset is a different
    /// crate (`cfgd-extras`) — its name contains the `cfgd` chunk, which the
    /// pre-substitution literal fallback would have accepted.
    #[test]
    fn binstall_matcher_rejects_literal_subset_false_match() {
        // After substitution the only surviving token is `{ target }`, which
        // resolves; the result is a concrete filename that must equal the
        // produced name — and `cfgd-1.0.0-…` != `cfgd-extras-1.0.0-…`.
        assert!(
            !binstall_asset_matches(
                "{ name }-{ version }-{ target }.tar.gz",
                "cfgd-extras-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &Some("x86_64-unknown-linux-gnu".to_string()),
                "cfgd",
                "1.0.0",
            ),
            "substituting name/version must reject a wrong asset whose stem merely \
             contains the literal chunks"
        );
    }

    /// `{ target-arch }` (cargo-binstall derives it from
    /// `target_lexicon::Architecture`; anodize does NOT hand-roll that mapping)
    /// stays on the literal-segment fallback: an unsubstituted-arch stem must
    /// still ACCEPT the right produced asset and REJECT a wrong-arch one (the
    /// 404 class), proving the fallback remains the safety net for the token
    /// anodize deliberately leaves alone.
    #[test]
    fn binstall_matcher_arch_token_falls_back_correctly() {
        let candidate = "{ name }-{ version }-{ target-arch }-unknown-linux-gnu.tar.gz";
        // Right arch stem: every literal chunk (cfgd, 1.0.0, -unknown-linux-gnu)
        // appears in the produced name → fallback accepts.
        assert!(
            binstall_asset_matches(
                candidate,
                "cfgd-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                &Some("x86_64-unknown-linux-gnu".to_string()),
                "cfgd",
                "1.0.0",
            ),
            "the literal-segment fallback must accept the right-arch asset"
        );
        // Wrong stem: the produced asset is a windows triple; the literal
        // `-unknown-linux-gnu` chunk is absent → fallback rejects.
        assert!(
            !binstall_asset_matches(
                candidate,
                "cfgd-1.0.0-x86_64-pc-windows-msvc.tar.gz",
                &Some("x86_64-pc-windows-msvc".to_string()),
                "cfgd",
                "1.0.0",
            ),
            "the literal-segment fallback must reject a wrong-os/arch stem"
        );
    }

    /// An untokened multi-`-` version stem still works through the full
    /// `validate_binstall` entry point: a `{{ .Version }}`-rendered snapshot
    /// version (`0.4.0-SNAPSHOT-3d07f6c`) carries multiple `-` and must still
    /// match the produced asset that embeds the same stem.
    #[test]
    fn binstall_untokened_multi_dash_version_stem_matches() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-{ target }.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        ctx.template_vars_mut()
            .set("Version", "0.4.0-SNAPSHOT-3d07f6c");
        ctx.template_vars_mut()
            .set("RawVersion", "0.4.0-SNAPSHOT-3d07f6c");
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-0.4.0-SNAPSHOT-3d07f6c-x86_64-unknown-linux-gnu.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        validate_binstall(&ctx, &cfg, &bs, &log())
            .expect("a multi-dash snapshot version stem must still match its produced asset");
    }

    /// A per-target override pointing at a missing asset must fail, naming the
    /// override triple.
    #[test]
    fn binstall_override_pointing_at_missing_asset_fails() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "aarch64-apple-darwin".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-darwin-wrongarch.tar.gz"
                        .to_string(),
                ),
                ..Default::default()
            },
        );
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            overrides: Some(overrides),
            ..Default::default()
        });
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "aarch64-apple-darwin",
            "cfgd-1.0.0-aarch64-apple-darwin.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        let err = validate_binstall(&ctx, &cfg, &bs, &log())
            .expect_err("override pointing at missing asset must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("aarch64-apple-darwin"),
            "names the triple: {msg}"
        );
    }

    /// Sharded/single-target snapshot: an auto-derived override for a triple the
    /// crate IS configured to build but that was NOT built in this run must be
    /// skipped (no archive to cross-check), not flagged as a 404. The override's
    /// asset name is correct; it simply isn't present in this shard.
    #[test]
    fn binstall_override_for_configured_unbuilt_triple_skips() {
        let mut overrides = BTreeMap::new();
        // Correct name for a darwin asset.
        overrides.insert(
            "aarch64-apple-darwin".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/o/cfgd/releases/download/v{ version }/cfgd-{ version }-darwin-arm64.tar.gz"
                        .to_string(),
                ),
                ..Default::default()
            },
        );
        let mut cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            overrides: Some(overrides),
            ..Default::default()
        });
        // The crate is configured to build darwin-arm64 (among others) ...
        cfg.builds = Some(vec![BuildConfig {
            targets: Some(vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]),
            ..Default::default()
        }]);
        let mut ctx = scoped_ctx(cfg.clone());
        // ... but THIS run produced only the linux asset (single-target shard).
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-linux-amd64.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        validate_binstall(&ctx, &cfg, &bs, &log())
            .expect("a configured-but-unbuilt triple must be skipped, not flagged");
    }

    /// An override for a triple the crate is NOT configured to build is a
    /// genuine misconfiguration and must still fail (the auto-derivation never
    /// produces these; only a hand-written override could).
    #[test]
    fn binstall_override_for_unconfigured_triple_fails() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "riscv64gc-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/o/cfgd/releases/download/v{ version }/cfgd-{ version }-linux-riscv64.tar.gz"
                        .to_string(),
                ),
                ..Default::default()
            },
        );
        let mut cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            overrides: Some(overrides),
            ..Default::default()
        });
        // Configured targets do NOT include riscv64.
        cfg.builds = Some(vec![BuildConfig {
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        }]);
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-1.0.0-linux-amd64.tar.gz",
        );
        let bs = cfg.binstall.clone().unwrap();
        let err = validate_binstall(&ctx, &cfg, &bs, &log())
            .expect_err("override for an unconfigured triple must fail");
        assert!(
            format!("{err}").contains("never builds"),
            "error names the never-built triple: {err}"
        );
    }

    // -- version-sync -------------------------------------------------------

    fn version_sync_crate() -> CrateConfig {
        CrateConfig {
            name: "cfgd".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            version_sync: Some(VersionSyncConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A crate whose scoped `Version` is empty (no resolvable tag) must fail
    /// version-sync validation in snapshot — today it is silently skipped.
    #[test]
    fn version_sync_empty_version_fails() {
        let cfg = version_sync_crate();
        let mut ctx = scoped_ctx(cfg.clone());
        ctx.template_vars_mut().unset("Version");
        ctx.template_vars_mut().unset("RawVersion");
        let err =
            validate_version_sync(&ctx, &cfg).expect_err("empty version must fail in snapshot");
        assert!(format!("{err}").contains("empty version"), "{err}");
    }

    /// A crate whose scoped version is not parseable semver must fail.
    #[test]
    fn version_sync_unparseable_version_fails() {
        let cfg = version_sync_crate();
        let mut ctx = scoped_ctx(cfg.clone());
        ctx.template_vars_mut().set("Version", "not-a-version");
        ctx.template_vars_mut().set("RawVersion", "not-a-version");
        let err = validate_version_sync(&ctx, &cfg)
            .expect_err("unparseable version must fail in snapshot");
        assert!(format!("{err}").contains("not parseable"), "{err}");
    }

    #[test]
    fn version_sync_valid_version_passes() {
        let cfg = version_sync_crate();
        let ctx = scoped_ctx(cfg.clone());
        validate_version_sync(&ctx, &cfg).expect("valid version passes");
    }

    // -- nix ----------------------------------------------------------------

    fn nix_crate() -> CrateConfig {
        nix_crate_named("cfgd")
    }

    fn nix_crate_named(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("o".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    license: Some("mit".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// All produced assets present and mapped — nix snapshot validation passes.
    #[test]
    fn nix_systems_all_backed_passes() {
        let cfg = nix_crate();
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-linux-amd64.tar.gz",
        );
        add_archive(
            &mut ctx,
            "cfgd",
            "aarch64-apple-darwin",
            "cfgd-darwin-arm64.tar.gz",
        );
        validate_nix(&mut ctx, &cfg, &log()).expect("all systems backed passes");
    }

    /// A `url_template` that resolves to an asset name the release does not
    /// produce maps a system to a missing asset — nix snapshot validation MUST
    /// fail, naming the system + missing asset.
    #[test]
    fn nix_system_mapped_to_missing_asset_fails() {
        let mut cfg = nix_crate();
        // Force the derivation URL to a name no produced archive carries.
        if let Some(nc) = cfg.publish.as_mut().and_then(|p| p.nix.as_mut()) {
            nc.url_template = Some(
                "https://github.com/o/cfgd/releases/download/v{{ version }}/cfgd-{{ arch }}-WRONG.tar.gz"
                    .to_string(),
            );
        }
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-linux-amd64.tar.gz",
        );
        let err =
            validate_nix(&mut ctx, &cfg, &log()).expect_err("system->missing-asset must fail");
        let msg = format!("{err}");
        assert!(msg.contains("x86_64-linux"), "names the nix system: {msg}");
        assert!(msg.contains("does not produce"), "explains the gap: {msg}");
    }

    /// A nix-configured crate that produced ZERO archives in this snapshot
    /// shard (the `--dry-run` / `--single-target` case where the archive stage
    /// emits nothing) must SKIP nix emission validation, not bail. The render
    /// twin (`render_nix_for_validation` -> `build_archive_tuples`) hard-errors
    /// on an empty archive set — correct for the real publish, wrong for the
    /// validation pre-flight, which has no assets to cross-check.
    #[test]
    fn nix_no_produced_archives_skips() {
        let cfg = nix_crate();
        let mut ctx = scoped_ctx(cfg.clone());
        // No `add_archive` calls — the crate produced nothing this shard.
        validate_nix(&mut ctx, &cfg, &log()).expect("zero produced archives must skip, not bail");
    }

    /// A target-restricted build (`--targets=`, the sharded determinism harness)
    /// must SKIP the whole emission-validate pass. The partial asset set cannot
    /// satisfy a cross-platform publisher, so validating it always fails — the
    /// exact failure that broke the v0.6.0 macOS + windows-aarch64 shards
    /// (`nix: no Linux/Darwin archive`, `schema-validate publisher 'homebrew'`).
    /// Here a deliberately-broken nix url would fail outside a restricted build;
    /// with `partial_target` set the pass steps aside before reaching it.
    #[test]
    fn target_restricted_build_skips_whole_emission_validate() {
        use anodizer_core::partial::PartialTarget;
        let mut cfg = nix_crate();
        if let Some(nc) = cfg.publish.as_mut().and_then(|p| p.nix.as_mut()) {
            nc.url_template = Some(
                "https://github.com/o/cfgd/releases/download/v{{ version }}/cfgd-{{ arch }}-WRONG.tar.gz"
                    .to_string(),
            );
        }
        let mut ctx = scoped_ctx(cfg.clone());
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-linux-amd64.tar.gz",
        );
        ctx.options.partial_target = Some(PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
        ]));
        validate_snapshot_emissions(&mut ctx, &log())
            .expect("target-restricted build must skip emission-validate, not fail");
    }

    /// Per-crate mode: two nix-configured crates share one snapshot run, but only
    /// `built` produced archives (the sharded case where one crate's targets land
    /// in this shard and another's do not). The skip is keyed on each crate's own
    /// `produced_archives` set, so it must not leak across crates: `empty` skips
    /// while `built` still runs its full cross-check and catches a 404 mismatch.
    /// A regression in per-crate isolation (e.g. seeing the sibling's archives)
    /// would either wrongly fail the empty crate or wrongly pass the built one.
    #[test]
    fn nix_per_crate_mode_skip_does_not_leak_across_crates() {
        // `built` carries a url_template that resolves to an asset name no
        // produced archive matches — its cross-check MUST fail even though a
        // sibling crate contributed archives to the same run.
        let mut built = nix_crate_named("built");
        if let Some(nc) = built.publish.as_mut().and_then(|p| p.nix.as_mut()) {
            nc.url_template = Some(
                "https://github.com/o/built/releases/download/v{{ version }}/built-{{ arch }}-WRONG.tar.gz"
                    .to_string(),
            );
        }
        let empty = nix_crate_named("empty");

        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![built.clone(), empty.clone()])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        // Only `built` produced an archive this shard; `empty` produced nothing.
        add_archive(
            &mut ctx,
            "built",
            "x86_64-unknown-linux-gnu",
            "built-linux-amd64.tar.gz",
        );

        // `empty` produced nothing of its own — must skip, never borrowing
        // `built`'s archive.
        validate_nix(&mut ctx, &empty, &log())
            .expect("empty crate produced nothing this shard; must skip, not borrow a sibling's");

        // `built` produced an archive but maps it to a WRONG asset — its full
        // cross-check must still fire and bail, unaffected by the sibling skip.
        let err = validate_nix(&mut ctx, &built, &log())
            .expect_err("built crate's 404 mismatch must fail despite a sibling skipping");
        assert!(
            format!("{err}").contains("does not produce"),
            "built crate's cross-check must run, not skip: {err}"
        );
    }

    // -- entry point --------------------------------------------------------

    /// Outside snapshot/dry-run the validator is a no-op even with a broken
    /// emission — the real release stages own validation there.
    #[test]
    fn validate_is_noop_outside_snapshot() {
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-broken.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        });
        let mut ctx = TestContextBuilder::new().crates(vec![cfg]).build();
        validate_snapshot_emissions(&mut ctx, &log()).expect("no-op outside snapshot/dry-run");
    }

    // -- version-dimension fix (snapshot false-positive) --------------------

    /// Build a snapshot context whose produced archives carry the synthesized
    /// snapshot version (`<base>-SNAPSHOT-<sha>`) while the global `Version`
    /// var is that same snapshot version — exactly the real `task snapshot`
    /// shape. The crate's binstall override is CORRECT (go-arch asset name +
    /// `{{ .Version }}`). The per-crate tag resolves to the numeric base.
    fn snapshot_version_ctx() -> (Context, CrateConfig) {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/o/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                ..Default::default()
            },
        );
        let cfg = binstall_crate(BinstallConfig {
            enabled: Some(true),
            overrides: Some(overrides),
            ..Default::default()
        });
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![cfg.clone()])
            .build();
        // The run's artifact-naming version, as `apply_snapshot_template_vars`
        // would have stamped it on the global `Version`.
        ctx.template_vars_mut()
            .set("Version", "0.5.0-SNAPSHOT-421f4c3");
        ctx.template_vars_mut().set("RawVersion", "0.5.0");
        ctx.template_vars_mut().set("ProjectName", "cfgd");
        // The produced archive carries the snapshot-suffixed version.
        add_archive(
            &mut ctx,
            "cfgd",
            "x86_64-unknown-linux-gnu",
            "cfgd-0.5.0-SNAPSHOT-421f4c3-linux-amd64.tar.gz",
        );
        (ctx, cfg)
    }

    /// BEFORE-FIX behavior pin: rendering the cross-check with the per-crate
    /// TAG version (`0.5.0`) against the snapshot-named asset is exactly the
    /// false-positive — `{{ .Version }}` renders `0.5.0`, the asset carries
    /// `0.5.0-SNAPSHOT-421f4c3`, no match → fail. This is what the old code
    /// did, and what the fix must stop doing.
    #[test]
    fn binstall_tag_version_against_snapshot_asset_false_positives() {
        let (mut ctx, cfg) = snapshot_version_ctx();
        // Simulate the pre-fix scope: Version = the tag-derived numeric base.
        ctx.template_vars_mut().set("Version", "0.5.0");
        let bs = cfg.binstall.clone().unwrap();
        let err = validate_binstall(&ctx, &cfg, &bs, &log()).expect_err(
            "pre-fix: tag version vs snapshot-named asset must (wrongly) fail — \
             proving the version dimension was the false-positive source",
        );
        assert!(format!("{err}").contains("404"), "{err}");
    }

    /// AFTER-FIX: the full entry point swaps `Version` to the artifact-naming
    /// snapshot version for the asset cross-check, so the CORRECT override
    /// PASSES even though the produced assets carry the `-SNAPSHOT-<sha>`
    /// suffix. Fails before the fix, passes after.
    #[test]
    fn snapshot_correct_override_passes_despite_snapshot_version_suffix() {
        let (mut ctx, _cfg) = snapshot_version_ctx();
        // Inject a fixed tag (numeric base) so version-sync's per-crate scope
        // resolves without a git fixture; the asset cross-check then swaps in
        // the snapshot version captured from the global `Version`.
        let resolver = |_: &Context, _: &CrateConfig| Some("0.5.0".to_string());
        validate_snapshot_emissions_with_resolver(&mut ctx, &log(), &resolver)
            .expect("correct override must pass once the cross-check uses the snapshot version");
    }
}
