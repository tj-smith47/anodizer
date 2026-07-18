use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{self, OsArtifact};

use super::*;

/// Convert `OsArtifact`s into `KrewPlatform`s.
///
/// When an artifact has arch "all", it is expanded into platform entries
/// for both amd64 and arm64.
///
/// `bin:` resolution per platform:
/// 1. Use the artifact's in-archive binary name when known
///    (`OsArtifact.binary`, populated from `extra_binaries[0]` for archives
///    or the `binary` metadata for uploadable binaries).
/// 2. Fall back to `default_binary_name` (the crate name) when the artifact
///    didn't carry a binary name.
/// 3. Append `.exe` for Windows targets when the resolved name doesn't
///    already end in `.exe`. Krew takes `bin:` literally — it does NOT
///    add `.exe` itself — so a Windows entry without the suffix fails to
///    install (krew validator: "source binary cannot be found in extracted
///    archive"). The `.exe` suffix is produced naturally because the
///    builder appends it to `binary.Name`; anodizer's archive metadata
///    stores the suffix-less name, so we normalize here.
pub(super) fn artifacts_to_platforms(
    artifacts: &[OsArtifact],
    default_binary_name: &str,
) -> Vec<KrewPlatform> {
    fn resolve_bin(a: &OsArtifact, default: &str, target_os: &str) -> String {
        let base = a.binary.clone().unwrap_or_else(|| default.to_string());
        if target_os == "windows" && !base.to_ascii_lowercase().ends_with(".exe") {
            format!("{}.exe", base)
        } else {
            base
        }
    }

    let mut platforms = Vec::new();
    for a in artifacts {
        let os = krew_os(&a.os).to_string();
        let bin = resolve_bin(a, default_binary_name, &os);
        let files = derive_krew_files(a, &bin);
        if a.arch == "all" {
            // Expand "all" into amd64 + arm64 entries
            for expanded_arch in &["amd64", "arm64"] {
                platforms.push(KrewPlatform {
                    os: os.clone(),
                    arch: expanded_arch.to_string(),
                    url: a.url.clone(),
                    sha256: a.sha256.clone(),
                    bin: bin.clone(),
                    files: files.clone(),
                });
            }
        } else {
            platforms.push(KrewPlatform {
                arch: krew_arch(&a.arch).to_string(),
                url: a.url.clone(),
                sha256: a.sha256.clone(),
                bin: bin.clone(),
                files: files.clone(),
                os,
            });
        }
    }
    platforms
}

/// Derive the per-platform `files:` extraction list for a krew platform entry.
///
/// Mirrors how every real krew-index plugin (ctx / ns / tree / access-matrix)
/// shapes `files:` — a `from`/`to` pair per file to lift out of the downloaded
/// archive, with `to: "."` flattening everything to the plugin install root
/// (which is why `bin:` references the flat binary name):
///
/// 1. **Binary** — always emitted. `from` is the binary's path *inside the
///    archive*: `<wrap_in_directory>/<bin>` for a nested archive, else `<bin>`.
///    Without this entry krew's default extractor can fail to find a nested
///    binary ("source binary cannot be found in extracted archive").
/// 2. **LICENSE** — emitted once when the archive bundles a license file
///    (gated on `OsArtifact.archive_files`, the actual archive contents), with
///    `from` carrying its real in-archive path (wrap prefix included). When the
///    basename isn't one krew accepts (`(?i)^(LICENSE|COPYING)(\.txt)?$`,
///    enforced by validate-krew-manifest), `to: "LICENSE"` renames it on
///    extraction so the install dir always carries a krew-accepted license;
///    a candidate already carrying an accepted name is preferred and kept flat.
/// 3. **README** (`*.md`) — emitted for each bundled markdown doc, same gating.
///
/// `bin` is the already-resolved install-dir binary name (`.exe`-suffixed on
/// Windows). The `from` path re-derives the suffix-aware in-archive name from
/// it so the Windows `.exe` handling carries into the extraction list.
pub(super) fn derive_krew_files(a: &OsArtifact, bin: &str) -> Vec<KrewFileEntry> {
    /// Join the `wrap_in_directory` prefix onto an in-archive file name,
    /// normalising to forward slashes (archive paths are always `/`-separated).
    fn in_archive_path(wrap: Option<&str>, name: &str) -> String {
        match wrap {
            Some(prefix) if !prefix.is_empty() => {
                format!("{}/{}", prefix.trim_end_matches('/'), name)
            }
            _ => name.to_string(),
        }
    }

    let wrap = a.wrap_in_directory.as_deref();
    let mut files = vec![KrewFileEntry {
        from: in_archive_path(wrap, bin),
        to: ".".to_string(),
    }];

    // LICENSE: emit exactly one entry, like real plugins do. Candidates are
    // sorted so the pick is stable regardless of archive listing order, and a
    // file already carrying a krew-accepted name wins over suffixed variants
    // (LICENSE-MIT / LICENSE-APACHE).
    let mut license_candidates: Vec<&String> = a
        .archive_files
        .iter()
        .filter(|p| is_license(basename(p)))
        .collect();
    license_candidates.sort();
    let license_path = license_candidates
        .iter()
        .find(|p| is_krew_accepted_license(basename(p)))
        .or_else(|| license_candidates.first())
        .map(|p| (*p).clone());
    if let Some(license) = license_path {
        // krew's validate-krew-manifest only accepts an installed license
        // named (?i)^(LICENSE|COPYING)(\.txt)?$; any other basename must be
        // renamed on extraction or the plugin fails krew-index validation.
        let to = if is_krew_accepted_license(basename(&license)) {
            "."
        } else {
            "LICENSE"
        };
        files.push(KrewFileEntry {
            from: license,
            to: to.to_string(),
        });
    }

    // README / markdown docs: include each bundled `*.md` (typically README.md),
    // but NOT the changelog (`CHANGELOG.md` is excluded from the krew install)
    // and NOT a `LICENSE.md` already selected above (it matches both the license
    // glob and `*.md`, which would otherwise duplicate the entry).
    for md in a.archive_files.iter().filter(|p| {
        let b = basename(p).to_ascii_lowercase();
        b.ends_with(".md") && !b.starts_with("changelog") && !is_license(basename(p))
    }) {
        files.push(KrewFileEntry {
            from: md.clone(),
            to: ".".to_string(),
        });
    }

    files
}

/// Whether an in-archive file basename is a license file (case-insensitive),
/// e.g. `LICENSE`, `license.txt`, `LICENSE.md`, `LICENSE-MIT`, `COPYING`.
fn is_license(basename: &str) -> bool {
    let b = basename.to_ascii_lowercase();
    b.starts_with("license") || b.starts_with("copying")
}

/// Whether a basename is one krew's validate-krew-manifest accepts as the
/// installed license file: `(?i)^(LICENSE|COPYING)(\.txt)?$`.
fn is_krew_accepted_license(basename: &str) -> bool {
    matches!(
        basename.to_ascii_lowercase().as_str(),
        "license" | "license.txt" | "copying" | "copying.txt"
    )
}

/// The final path component of a `/`-separated in-archive path.
pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// publish_to_krew
// ---------------------------------------------------------------------------

/// Per-crate outcome returned by [`publish_to_krew`].
///
/// `pushed` flags whether the run made a real upstream side effect
/// (the `PrDirect` flow's branch push + PR open). Drives the caller's
/// `any_pushed` gate that decides whether to populate rollback evidence.
/// The `BotWebhook` flow always leaves `pushed = false`: the
/// krew-release-bot server owns the krew-index PR, so anodizer has
/// nothing to roll back.
#[derive(Debug, Default, Clone)]
pub struct KrewPublishOutcome {
    /// `true` when the `PrDirect` flow pushed a branch + opened a PR.
    /// The caller's `any_pushed` gate checks this.
    pub pushed: bool,
}

impl KrewPublishOutcome {
    /// Convenience constructor for run paths that exit before reaching
    /// the webhook / push branches.
    pub(super) fn skipped() -> Self {
        Self { pushed: false }
    }
}

/// Whether `crate_name` has at least one krew-eligible archive artifact under
/// `krew_cfg` in this run.
///
/// Routes through the same `find_all_platform_artifacts_with_variant` collector
/// the live publish uses (honoring the `ids` allow-list and the
/// amd64/arm microarchitecture-variant filters), so the eligibility predicate is
/// one source of truth: the live path errors when this would be `false` (no
/// archive to construct the manifest from), and the offline schema validator
/// skips the crate on the same signal. A single-target / sharded snapshot that
/// built no archive for this crate therefore yields `false` here rather than
/// tripping the publisher's "no archive artifacts" guard.
pub(crate) fn crate_has_krew_artifacts(
    ctx: &Context,
    crate_name: &str,
    krew_cfg: &anodizer_core::config::KrewConfig,
) -> Result<bool> {
    let ids_filter = krew_cfg.ids.as_deref();
    let amd64_variant = krew_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = krew_cfg.arm_variant.as_deref();
    let artifacts = util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        arm_variant,
    )?;
    Ok(!artifacts.is_empty())
}

/// Resolve a crate's krew config and render its plugin manifest in-memory, with
/// no clone, disk, or network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip`,
/// `skip_upload`, or a falsy `if` condition). Errors when the crate carries no
/// `krew` block, when a required narrative field (description / short
/// description) is unset, when an archive carries more than one binary (krew
/// allows exactly one per platform), when no eligible archive artifact exists,
/// or when a matched archive is missing its `sha256` metadata. The live publish
/// path and the offline schema validator both call this so the validated
/// document is byte-for-byte what a real publish would push.
pub(crate) fn render_krew_manifest_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "krew")?;
    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    // Honor `skip` first (template-aware), then the falsy-`if` gate, then
    // `skip_upload` — the same order and short-circuit the live publish applies,
    // so a skipped crate yields `None` (nothing to render or validate).
    if let Some(d) = krew_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("krew: render skip template for '{}'", crate_name))?;
        if off {
            return Ok(None);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        krew_cfg.if_condition.as_deref(),
        &format!("krew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        return Ok(None);
    }
    if util::should_skip_upload(krew_cfg.skip_upload.as_ref(), ctx, log, None)? {
        return Ok(None);
    }

    let version = ctx.version();

    // Validate required narrative fields before proceeding, falling back to
    // `metadata.description` when the krew config leaves them unset.
    let effective_description: Option<&str> = krew_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name));
    if effective_description.is_none_or(str::is_empty) {
        anyhow::bail!("krew: manifest description is not set for '{}'", crate_name);
    }
    // `short_description` is a krew-required tagline with no Cargo.toml
    // counterpart; fall back to the (possibly Cargo.toml-derived) description
    // so a plain Rust project does not hard-error on it.
    if krew_cfg
        .short_description
        .as_ref()
        .is_none_or(|s| s.is_empty())
        && effective_description.is_none_or(str::is_empty)
    {
        anyhow::bail!(
            "krew: manifest short_description is not set for '{}'",
            crate_name
        );
    }

    let description_raw = krew_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description = util::render_or_warn(ctx, log, "krew.description", description_raw)?;
    let short_description_raw = krew_cfg
        .short_description
        .as_deref()
        .or(effective_description)
        .unwrap_or(crate_name);
    let short_description =
        util::render_or_warn(ctx, log, "krew.short_description", short_description_raw)?;
    warn_if_short_description_too_long(&short_description, crate_name, log);
    // Derive GitHub slug (owner/repo) for the homepage fallback, consistent with
    // the homebrew publisher.
    let plugin_github = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| (gh.owner.clone(), gh.name.clone()));
    let github_slug = plugin_github
        .as_ref()
        .map(|(owner, name)| format!("{}/{}", owner, name));
    // The homepage fallback's final arm needs the krew-index repo owner; resolve
    // it the same way the live path does, but only for the fallback (no error
    // when the repository block is absent and another fallback already applies).
    // The live path requires the repository block before rendering, so on that
    // path the owner is always present and this matches its output. An empty /
    // absent owner (only reachable via the offline validator, which does not
    // require the block) drops the final arm rather than emit a degenerate
    // `https://github.com//crate` URL — never widen leniency past the live
    // path's guarantees.
    let repo_owner_fallback = crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
        .map(|(owner_raw, _)| util::render_or_warn(ctx, log, "krew.repository.owner", &owner_raw))
        .transpose()?
        .filter(|owner| !owner.is_empty());
    let homepage_raw = krew_cfg
        .homepage
        .clone()
        .or_else(|| ctx.config.meta_homepage_for(crate_name).map(str::to_string))
        .or_else(|| {
            github_slug
                .as_deref()
                .map(|slug| format!("https://github.com/{}", slug))
        })
        .or_else(|| {
            repo_owner_fallback
                .as_deref()
                .map(|owner| format!("https://github.com/{}/{}", owner, crate_name))
        })
        .unwrap_or_default();
    let homepage = ctx
        .render_template(&homepage_raw)
        .with_context(|| format!("krew: render homepage template for '{}'", crate_name))?;
    let caveats_raw = krew_cfg.caveats.clone().unwrap_or_default();
    let caveats = ctx
        .render_template(&caveats_raw)
        .with_context(|| format!("krew: render caveats template for '{}'", crate_name))?;

    // Find artifacts across all platforms, applying the IDs +
    // amd64_variant/arm_variant filters.
    let ids_filter = krew_cfg.ids.as_deref();
    let amd64_variant = krew_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = krew_cfg.arm_variant.as_deref();

    // Krew plugins support a single binary per archive. Walk the eligible
    // archives — through the SAME `ids` allow-list `find_all_platform_artifacts_with_variant`
    // applies (via the shared `filter_by_ids`), never a hand-rolled inline copy —
    // so an `ids`-excluded archive's binary count is not mistakenly enforced.
    let archives = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name);
    for archive in util::filter_by_ids(archives, ids_filter) {
        let binary_count = archive.extra_binaries().len();
        if binary_count != 1 {
            anyhow::bail!(
                "krew: only one binary per archive allowed, got {} on {:?}",
                binary_count,
                archive.name
            );
        }
    }

    let all_artifacts: Vec<OsArtifact> = util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        arm_variant,
    )?
    .into_iter()
    // Krew installs only on linux/darwin/windows. Drop Apple-but-not-macOS
    // archives here so the `is_empty()` guard below still fires on a build
    // whose only Apple target was watchos/tvos (os=darwin) or ios — otherwise
    // the manifest would ship a `darwin`/`ios` platform selector for a binary
    // that cannot run there. Mirrors homebrew/nix `is_macos` eligibility.
    .filter(krew_eligible)
    .collect();

    let url_template = krew_cfg.url_template.as_deref();

    if all_artifacts.is_empty() {
        // An empty archive set is a hard error — a krew manifest with no real
        // artifacts is unusable (a placeholder URL produces 404s on install).
        anyhow::bail!(
            "krew: no archive artifacts found for '{}'. The krew publisher \
             needs at least one platform archive to construct the manifest. \
             Either add Windows/Linux/macOS targets for this crate or remove \
             the krew publisher config.",
            crate_name
        );
    }
    // krew's `addURIAndSha` validator rejects manifests whose
    // `spec.platforms[].sha256` is empty ("Hash validation failed"). Empty
    // sha256 metadata would silently produce an unusable plugin manifest.
    if let Some(empty) = all_artifacts.iter().find(|a| a.sha256.is_empty()) {
        anyhow::bail!(
            "krew: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated krew plugin \
             manifest would embed an empty `sha256:` field, which krew \
             rejects at install time. Check dist/artifacts.json for the \
             archive entry's metadata.sha256, and re-run `task release` from \
             a clean dist/ if the field is absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }
    let platforms = {
        let mut plats = artifacts_to_platforms(&all_artifacts, crate_name);
        if let Some(tmpl) = url_template {
            for p in &mut plats {
                p.url = util::render_url_template_with_ctx(
                    ctx, tmpl, crate_name, &version, &p.arch, &p.os,
                );
            }
        }
        plats
    };

    // Resolve the plugin name (honoring the `krew.name` override) so the
    // manifest `metadata.name` carries the same value the live path stamps onto
    // the published file basename and the webhook `pluginName`.
    let plugin_name_rendered = resolve_plugin_name(krew_cfg.name.as_deref(), crate_name, |t| {
        ctx.render_template(t)
    })?;
    let plugin_name = plugin_name_rendered.as_str();

    let manifest = generate_manifest(&KrewManifestParams {
        name: plugin_name,
        version: &version,
        homepage: &homepage,
        short_description: &short_description,
        description: &description,
        caveats: &caveats,
        platforms: &platforms,
    })?;

    Ok(Some(manifest))
}
